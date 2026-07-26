use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    browser::{BrowserIssueKind, PlaywrightEvidence},
    command_runner::{CommandRunner, EnvironmentPolicy},
    jobs::{self, now, write_json, CheckExecution, CompletionGate, GateStatus, Job, JobState},
    project_config::ProjectConfig,
    runtime::RuntimeProcess,
    worker::{SharedWorker, WorkerRequest},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Approved,
    ChangesRequired,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub status: ReviewStatus,
    pub summary: String,
    #[serde(default)]
    pub criterion_findings: Vec<CriterionReviewFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionReviewFinding {
    pub criterion_id: String,
    pub status: ReviewStatus,
    pub summary: String,
}

#[async_trait]
pub trait Reviewer: Send + Sync {
    async fn review(&self, prompt: &str) -> Result<ReviewResult>;
}

pub type SharedReviewer = Arc<dyn Reviewer>;

#[derive(Debug, Clone)]
pub struct GemmaReviewer {
    pub base_url: String,
    pub model: String,
    client: reqwest::Client,
}

impl GemmaReviewer {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Reviewer for GemmaReviewer {
    async fn review(&self, prompt: &str) -> Result<ReviewResult> {
        let schema = serde_json::json!({
            "type":"object",
            "properties":{
                "status":{"type":"string","enum":["approved","changes_required","blocked"]},
                "summary":{"type":"string"},
                "criterion_findings":{
                    "type":"array",
                    "items":{
                        "type":"object",
                        "properties":{
                            "criterion_id":{"type":"string"},
                            "status":{"type":"string","enum":["approved","changes_required","blocked"]},
                            "summary":{"type":"string"}
                        },
                        "required":["criterion_id","status","summary"]
                    }
                }
            },
            "required":["status","summary","criterion_findings"]
        });
        let value: serde_json::Value = self
            .client
            .post(format!("{}/api/chat", self.base_url.trim_end_matches('/')))
            .json(&serde_json::json!({
                "model":self.model,
                "stream":false,
                "format":schema,
                "messages":[{"role":"user","content":prompt}]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let content = value
            .pointer("/message/content")
            .and_then(|value| value.as_str())
            .context("reviewer did not return message.content")?;
        #[derive(Deserialize)]
        struct RawReview {
            status: String,
            summary: String,
            #[serde(default)]
            criterion_findings: Vec<RawCriterionFinding>,
        }
        #[derive(Deserialize)]
        struct RawCriterionFinding {
            criterion_id: String,
            status: String,
            summary: String,
        }
        let raw: RawReview = serde_json::from_str(content)?;
        let status = parse_review_status(&raw.status)?;
        Ok(ReviewResult {
            status,
            summary: raw.summary,
            criterion_findings: raw
                .criterion_findings
                .into_iter()
                .map(|finding| {
                    Ok(CriterionReviewFinding {
                        criterion_id: finding.criterion_id,
                        status: parse_review_status(&finding.status)?,
                        summary: finding.summary,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

fn parse_review_status(status: &str) -> Result<ReviewStatus> {
    match status {
        "approved" => Ok(ReviewStatus::Approved),
        "changes_required" => Ok(ReviewStatus::ChangesRequired),
        "blocked" => Ok(ReviewStatus::Blocked),
        _ => anyhow::bail!("reviewer returned an unsupported status"),
    }
}

#[derive(Clone)]
pub struct Orchestrator {
    workspace: std::path::PathBuf,
    config: ProjectConfig,
    worker: SharedWorker,
    reviewer: Option<SharedReviewer>,
    runner: CommandRunner,
}

impl Orchestrator {
    pub fn new(
        workspace: impl AsRef<Path>,
        config: ProjectConfig,
        worker: SharedWorker,
        reviewer: Option<SharedReviewer>,
    ) -> Result<Self> {
        let workspace = workspace.as_ref().canonicalize()?;
        Ok(Self {
            workspace,
            config,
            worker,
            reviewer,
            runner: CommandRunner::default(),
        })
    }

    pub async fn run(&self, mut job: Job) -> Result<Job> {
        if job.record.state != JobState::Approved {
            anyhow::bail!("job must be approved before work begins");
        }
        if self.config.completion.required_checks.is_empty() {
            job.transition(
                JobState::Blocked,
                "No required project checks are configured",
            )?;
            return Ok(job);
        }
        for check in &self.config.completion.required_checks {
            if !self.config.commands.contains_key(check) {
                job.transition(
                    JobState::Blocked,
                    format!("Required check `{check}` has no approved argv command"),
                )?;
                return Ok(job);
            }
        }
        let ui_required = self
            .config
            .completion
            .required_checks
            .iter()
            .any(|check| check == &self.config.ui.test_command);
        if ui_required && self.config.runtime.is_none() {
            job.transition(
                JobState::Blocked,
                "UI testing is required but [runtime] is not configured",
            )?;
            return Ok(job);
        }

        let mut gates = jobs::default_gates(
            &self.config.completion.required_checks,
            self.config.completion.require_review,
        );
        if ui_required {
            for (name, required) in [
                (
                    "browser console errors",
                    self.config.ui.fail_on_console_error,
                ),
                ("browser page exceptions", true),
                ("browser page crashes", true),
                ("browser failed requests", true),
                ("Playwright assertions", true),
            ] {
                gates.push(CompletionGate {
                    name: name.into(),
                    required,
                    status: GateStatus::NotRun,
                    evidence: String::new(),
                    explanation: String::new(),
                    updated_at: now(),
                });
            }
        }
        job.set_gates(&gates)?;
        let mut session_id = None;
        let mut previous_failure = None::<String>;
        let mut repeated_failures = 0_u32;
        let mut check_history = Vec::<CheckExecution>::new();
        let mut final_summary = String::new();
        let mut changed_files = Vec::<String>::new();
        let mut repair_evidence = None::<String>;

        for attempt in 1..=self.config.completion.max_attempts {
            job.record.attempt = attempt;
            job.record.current_worker = Some(self.worker.name().to_string());
            job.transition(
                if attempt == 1 {
                    JobState::Working
                } else {
                    JobState::Fixing
                },
                format!("{} attempt {attempt}", self.worker.name()),
            )?;
            let prompt = self.worker_prompt(&job, attempt, repair_evidence.as_deref())?;
            let invocation_id = format!("{}-attempt-{attempt}", job.record.id);
            let result = match self
                .worker
                .run(WorkerRequest {
                    invocation_id,
                    job_id: job.record.id.clone(),
                    workspace: self.workspace.clone(),
                    artifacts: job.artifact_dir(),
                    prompt,
                    attempt,
                    session_id: session_id.clone(),
                    timeout_seconds: 1800,
                })
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    job.transition(JobState::Failed, format!("Worker failed: {error}"))?;
                    return Ok(job);
                }
            };
            crate::jobs::append_jsonl(&job.path("worker-invocations.jsonl"), &result.process)?;
            session_id = result.session_id.clone();
            final_summary = result.summary;
            changed_files.extend(result.changed_files);
            changed_files.sort();
            changed_files.dedup();
            write_json(&job.path("changed-files.json"), &changed_files)?;
            let modified_tests = changed_files
                .iter()
                .filter(|path| {
                    let lower = path.to_ascii_lowercase();
                    lower.contains("test") || lower.contains("spec")
                })
                .cloned()
                .collect::<Vec<_>>();
            if !modified_tests.is_empty() {
                job.event(
                    "tests_modified",
                    &format!(
                        "Worker modified test-related files: {}",
                        modified_tests.join(", ")
                    ),
                )?;
            }
            let has_patch =
                !changed_files.is_empty() || workspace_has_changes(&self.workspace).await;
            update_gate(
                &mut gates,
                "patch produced",
                if has_patch {
                    GateStatus::Passed
                } else {
                    GateStatus::Failed
                },
                if has_patch {
                    format!("Changed files: {}", changed_files.join(", "))
                } else {
                    "Worker reported no changed files and Git found no worktree changes".into()
                },
            );
            job.set_gates(&gates)?;
            if !has_patch {
                job.transition(JobState::Blocked, "Worker did not produce a patch")?;
                return Ok(job);
            }

            job.transition(JobState::Checking, "Running approved project checks")?;
            let mut failures = Vec::new();
            let mut runtime = None;
            if ui_required {
                let runtime_config = self.config.runtime.as_ref().expect("checked above");
                let start_argv = self.config.command(&runtime_config.start_command)?.to_vec();
                let readiness = runtime_config
                    .healthcheck_url
                    .as_deref()
                    .unwrap_or(&runtime_config.base_url);
                match RuntimeProcess::start(
                    &start_argv,
                    &self.workspace,
                    &job.artifact_dir(),
                    Some(readiness),
                    Duration::from_secs(runtime_config.startup_timeout_seconds),
                )
                .await
                {
                    Ok(process) => {
                        job.event(
                            "runtime",
                            &format!("Application ready with PID {}", process.pid()),
                        )?;
                        runtime = Some(process);
                    }
                    Err(error) => {
                        let evidence = format!("Application failed readiness: {error:#}");
                        update_gate(
                            &mut gates,
                            &format!("{} passed", self.config.ui.test_command),
                            GateStatus::Failed,
                            evidence.clone(),
                        );
                        failures.push(evidence);
                    }
                }
            }
            for category in &self.config.completion.required_checks {
                if category == &self.config.ui.test_command && runtime.is_none() {
                    continue;
                }
                let argv = self.config.command(category)?.to_vec();
                let result = self
                    .runner
                    .run(
                        format!("{}-check-{category}-{attempt}", job.record.id),
                        argv,
                        &self.workspace,
                        None,
                        Duration::from_secs(900),
                        EnvironmentPolicy::Check,
                    )
                    .await?;
                let evidence = result.evidence();
                if category == &self.config.ui.test_command {
                    let artifact_source = self.workspace.join("test-results");
                    let browser = PlaywrightEvidence::collect(
                        &result,
                        &artifact_source,
                        &self.config.ui.ignored_console_patterns,
                    )?;
                    browser.write(
                        &job.artifact_dir()
                            .join(format!("playwright-attempt-{attempt}.json")),
                    )?;
                    for (name, kind) in [
                        ("browser console errors", BrowserIssueKind::ConsoleError),
                        ("browser page exceptions", BrowserIssueKind::PageException),
                        ("browser page crashes", BrowserIssueKind::PageCrash),
                        ("browser failed requests", BrowserIssueKind::FailedRequest),
                        ("Playwright assertions", BrowserIssueKind::AssertionFailure),
                    ] {
                        let issues = browser.unignored(kind);
                        let status = if issues.is_empty() {
                            GateStatus::Passed
                        } else {
                            GateStatus::Failed
                        };
                        let detail = if issues.is_empty() {
                            format!(
                                "No unignored {kind:?} issues; {} ignored issue(s) remain recorded",
                                browser
                                    .issues
                                    .iter()
                                    .filter(|issue| issue.kind == kind && issue.ignored)
                                    .count()
                            )
                        } else {
                            issues
                                .iter()
                                .map(|issue| format!("{} {}", issue.location, issue.message))
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        update_gate(&mut gates, name, status, detail.clone());
                        if status == GateStatus::Failed
                            && gates
                                .iter()
                                .find(|gate| gate.name == name)
                                .is_some_and(|gate| gate.required)
                        {
                            failures.push(format!("{name}: {detail}"));
                        }
                    }
                }
                update_gate(
                    &mut gates,
                    &format!("{category} passed"),
                    if result.success() {
                        GateStatus::Passed
                    } else {
                        GateStatus::Failed
                    },
                    evidence.clone(),
                );
                if !result.success() {
                    failures.push(format!("Required check `{category}` failed:\n{evidence}"));
                }
                check_history.push(CheckExecution {
                    category: category.clone(),
                    result,
                });
            }
            if let Some(runtime) = runtime {
                if let Err(error) = runtime.stop().await {
                    failures.push(format!("Application cleanup failed: {error:#}"));
                } else {
                    job.event("runtime", "Application process group stopped")?;
                }
            }
            write_json(&job.path("check-results.json"), &check_history)?;
            let _ = job.apply_check_evidence(&check_history, &self.config.ui.test_command)?;
            job.set_gates(&gates)?;
            if failures.is_empty() {
                break;
            }
            let evidence = failures.join("\n\n");
            if previous_failure.as_deref() == Some(&evidence) {
                repeated_failures += 1;
            } else {
                repeated_failures = 0;
            }
            previous_failure = Some(evidence.clone());
            if repeated_failures >= 1 {
                job.transition(
                    JobState::Blocked,
                    "The same check failure repeated without progress",
                )?;
                return Ok(job);
            }
            if attempt == self.config.completion.max_attempts {
                job.transition(JobState::Blocked, "Maximum repair attempts reached")?;
                return Ok(job);
            }
            job.event("repair", "Check evidence returned to the coding worker")?;
            repair_evidence = Some(evidence);
        }

        if self.config.completion.require_review {
            let Some(reviewer) = &self.reviewer else {
                job.transition(
                    JobState::Blocked,
                    "Review is required but no reviewer is configured",
                )?;
                return Ok(job);
            };
            job.transition(JobState::Reviewing, "Independent review in progress")?;
            let review = reviewer
                .review(&self.review_prompt(&job, &check_history)?)
                .await?;
            write_json(&job.path("review.json"), &review)?;
            job.add_reviewer_findings(
                &review
                    .criterion_findings
                    .iter()
                    .map(|finding| {
                        (
                            finding.criterion_id.clone(),
                            format!("{:?}: {}", finding.status, finding.summary),
                        )
                    })
                    .collect::<Vec<_>>(),
            )?;
            update_gate(
                &mut gates,
                "review approved",
                if review.status == ReviewStatus::Approved {
                    GateStatus::Passed
                } else {
                    GateStatus::Failed
                },
                review.summary.clone(),
            );
            job.set_gates(&gates)?;
            if review.status != ReviewStatus::Approved {
                job.transition(
                    JobState::Blocked,
                    format!("Reviewer returned {:?}", review.status),
                )?;
                return Ok(job);
            }
        }

        let criteria_verified =
            job.apply_check_evidence(&check_history, &self.config.ui.test_command)?;
        update_gate(
            &mut gates,
            "acceptance criteria verified",
            if criteria_verified {
                GateStatus::Passed
            } else {
                GateStatus::Failed
            },
            if criteria_verified {
                "Every required evidence type is backed by a check that actually passed".into()
            } else {
                "One or more criteria lack required passing evidence".into()
            },
        );
        job.set_gates(&gates)?;
        if !criteria_verified {
            job.transition(
                JobState::Blocked,
                "One or more acceptance criteria remain unverified",
            )?;
            return Ok(job);
        }
        let report = format!(
            "# Job complete\n\n{}\n\nWorker: {}\nAttempts: {}\nChanged files:\n{}\n\nAll configured required completion gates passed.\n",
            final_summary,
            self.worker.name(),
            job.record.attempt,
            changed_files
                .iter()
                .map(|file| format!("- {file}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        job.finish(&report)?;
        Ok(job)
    }

    fn worker_prompt(&self, job: &Job, attempt: u32, failure: Option<&str>) -> Result<String> {
        let request = job.request()?;
        let specification = job.specification()?;
        let instructions = project_instructions(&self.workspace)?;
        let repair = failure
            .map(|failure| {
                format!(
                    "\nThis is repair attempt {attempt}. Bender independently ran the approved checks. Fix the implementation; do not disable legitimate tests.\n\nFailure evidence:\n{failure}\n"
                )
            })
            .unwrap_or_default();
        Ok(format!(
            "You are the primary coding worker controlled by Bender. Work only inside the current workspace. Inspect the repository, implement the approved specification, update legitimate tests, and return the requested structured summary. A successful exit does not complete the job; Bender runs independent gates.\n\nOriginal request:\n{request}\n\nApproved specification:\n{specification}\n\nProject instructions:\n{instructions}\n{repair}"
        ))
    }

    fn review_prompt(&self, job: &Job, checks: &[CheckExecution]) -> Result<String> {
        Ok(format!(
            "Review this completed software job independently. Return approved, changes_required, or blocked. Do not claim completion; Bender owns completion gates.\n\nRequest:\n{}\n\nSpecification:\n{}\n\nChecks:\n{}",
            job.request()?,
            job.specification()?,
            checks
                .iter()
                .map(|check| {
                    format!(
                        "category: {}\n{}",
                        check.category,
                        check.result.evidence()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        ))
    }
}

fn update_gate(gates: &mut [CompletionGate], name: &str, status: GateStatus, evidence: String) {
    if let Some(gate) = gates.iter_mut().find(|gate| gate.name == name) {
        gate.status = status;
        gate.evidence = evidence;
        gate.updated_at = now();
    }
}

async fn workspace_has_changes(workspace: &Path) -> bool {
    let runner = CommandRunner::default();
    runner
        .run(
            "git-worktree-evidence",
            vec![
                "git".into(),
                "status".into(),
                "--porcelain=v1".into(),
                "--untracked-files=all".into(),
            ],
            workspace,
            None,
            Duration::from_secs(10),
            EnvironmentPolicy::Check,
        )
        .await
        .is_ok_and(|result| result.success() && !result.stdout.trim().is_empty())
}

fn project_instructions(workspace: &Path) -> Result<String> {
    let mut sections = Vec::new();
    for relative in ["AGENTS.md", ".bender/instructions.md"] {
        let path = workspace.join(relative);
        if path.is_file() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            sections.push(format!("--- {relative} ---\n{content}"));
        }
    }
    Ok(if sections.is_empty() {
        "No project instruction files found; inspect the repository conventions.".to_string()
    } else {
        sections.join("\n\n")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackDecision {
    Accepted,
    Declined,
}

pub fn select_worker(
    codex_available: bool,
    fallback: Option<(FallbackDecision, SharedWorker)>,
    codex: SharedWorker,
) -> Result<SharedWorker> {
    if codex_available {
        return Ok(codex);
    }
    match fallback {
        Some((FallbackDecision::Accepted, worker)) => Ok(worker),
        Some((FallbackDecision::Declined, _)) => {
            anyhow::bail!("Codex is unavailable and Qwen fallback was declined")
        }
        None => anyhow::bail!("Codex is unavailable and no approved fallback is configured"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command_runner::CommandResult,
        jobs::{AcceptanceCriterion, JobStore},
        worker::{CodexCliWorker, CodingWorker, WorkerResult},
    };
    use std::{
        fs,
        sync::atomic::{AtomicU32, Ordering},
    };
    use tempfile::tempdir;

    struct FakeCodexWorker {
        calls: AtomicU32,
    }

    #[async_trait]
    impl CodingWorker for FakeCodexWorker {
        fn name(&self) -> &'static str {
            "fake_codex"
        }

        async fn run(&self, request: WorkerRequest) -> Result<WorkerResult> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            fs::write(
                request.workspace.join("result.txt"),
                if call == 1 { "incomplete" } else { "complete" },
            )?;
            Ok(WorkerResult {
                worker: self.name().into(),
                invocation_id: request.invocation_id.clone(),
                session_id: Some("fake-session".into()),
                summary: format!("fake attempt {call}"),
                changed_files: vec!["result.txt".into()],
                tests: vec![],
                process: fake_process(request.invocation_id, request.workspace),
            })
        }

        async fn cancel(&self, _invocation_id: &str) -> Result<bool> {
            Ok(true)
        }
    }

    struct FakeReviewer;

    #[async_trait]
    impl Reviewer for FakeReviewer {
        async fn review(&self, prompt: &str) -> Result<ReviewResult> {
            assert!(prompt.contains("result.txt") || prompt.contains("Checks"));
            Ok(ReviewResult {
                status: ReviewStatus::Approved,
                summary: "fake review approved".into(),
                criterion_findings: vec![CriterionReviewFinding {
                    criterion_id: "ac-1".into(),
                    status: ReviewStatus::Approved,
                    summary: "unit evidence confirms the result".into(),
                }],
            })
        }
    }

    struct FakeOllamaWorker;

    #[async_trait]
    impl CodingWorker for FakeOllamaWorker {
        fn name(&self) -> &'static str {
            "fake_ollama"
        }

        async fn run(&self, _request: WorkerRequest) -> Result<WorkerResult> {
            anyhow::bail!("not invoked by this selection test")
        }

        async fn cancel(&self, _invocation_id: &str) -> Result<bool> {
            Ok(false)
        }
    }

    struct StaticWorker;

    #[async_trait]
    impl CodingWorker for StaticWorker {
        fn name(&self) -> &'static str {
            "static_worker"
        }

        async fn run(&self, request: WorkerRequest) -> Result<WorkerResult> {
            fs::write(request.workspace.join("result.txt"), "incomplete")?;
            Ok(WorkerResult {
                worker: self.name().into(),
                invocation_id: request.invocation_id.clone(),
                session_id: Some("static-session".into()),
                summary: r#"{"status":"complete"}"#.into(),
                changed_files: vec!["result.txt".into()],
                tests: vec![],
                process: fake_process(request.invocation_id, request.workspace),
            })
        }

        async fn cancel(&self, _invocation_id: &str) -> Result<bool> {
            Ok(true)
        }
    }

    struct RejectingReviewer;

    #[async_trait]
    impl Reviewer for RejectingReviewer {
        async fn review(&self, _prompt: &str) -> Result<ReviewResult> {
            Ok(ReviewResult {
                status: ReviewStatus::ChangesRequired,
                summary: "acceptance criterion is not demonstrated".into(),
                criterion_findings: vec![],
            })
        }
    }

    fn fake_process(id: String, workspace: std::path::PathBuf) -> CommandResult {
        CommandResult {
            invocation_id: id,
            argv: vec!["fake".into()],
            working_directory: workspace,
            pid: 1,
            started_at: now(),
            finished_at: now(),
            elapsed_ms: 0,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            stdout: String::new(),
            stderr: String::new(),
            output_truncated: false,
        }
    }

    #[tokio::test]
    async fn end_to_end_failure_repair_review_and_completion() {
        let root = tempdir().unwrap();
        let test_script = root.path().join("check.sh");
        fs::write(
            &test_script,
            "#!/bin/sh\n[ \"$(cat result.txt)\" = complete ]\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&test_script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let store = JobStore::new(root.path()).unwrap();
        let mut job = store
            .create("make result complete", "local", Some("fixture".into()))
            .unwrap();
        job.set_specification(
            "Write complete to result.txt and preserve the check.",
            &[AcceptanceCriterion::new(
                "ac-1",
                "result.txt contains complete",
                ["unit_test"],
            )],
        )
        .unwrap();
        job.approve().unwrap();
        let mut config = ProjectConfig::default();
        config
            .commands
            .insert("unit".into(), vec![test_script.display().to_string()]);
        config.completion.required_checks = vec!["unit".into()];
        config.completion.max_attempts = 3;
        config.completion.require_review = true;
        let orchestrator = Orchestrator::new(
            root.path(),
            config,
            Arc::new(FakeCodexWorker {
                calls: AtomicU32::new(0),
            }),
            Some(Arc::new(FakeReviewer)),
        )
        .unwrap();

        let completed = orchestrator.run(job).await.unwrap();
        assert_eq!(
            completed.record.state,
            JobState::Complete,
            "{}",
            completed.record.message
        );
        assert_eq!(completed.record.attempt, 2);
        assert!(completed.all_required_gates_pass().unwrap());
        let events = fs::read_to_string(completed.path("events.jsonl")).unwrap();
        assert!(events.contains("Check evidence returned to the coding worker"));
        assert_eq!(
            fs::read_to_string(root.path().join("result.txt")).unwrap(),
            "complete"
        );
    }

    #[test]
    fn fallback_requires_explicit_acceptance() {
        let codex: SharedWorker = Arc::new(FakeCodexWorker {
            calls: AtomicU32::new(0),
        });
        let qwen: SharedWorker = Arc::new(FakeOllamaWorker);
        assert!(select_worker(
            false,
            Some((FallbackDecision::Declined, qwen.clone())),
            codex.clone()
        )
        .is_err());
        assert_eq!(
            select_worker(false, Some((FallbackDecision::Accepted, qwen)), codex)
                .unwrap()
                .name(),
            "fake_ollama"
        );
    }

    fn approved_job(root: &Path) -> Job {
        let store = JobStore::new(root).unwrap();
        let mut job = store.create("make result complete", "local", None).unwrap();
        job.set_specification(
            "Write complete to result.txt.",
            &[AcceptanceCriterion::new(
                "one",
                "result is complete",
                ["unit_test"],
            )],
        )
        .unwrap();
        job.approve().unwrap();
        job
    }

    #[tokio::test]
    async fn missing_checks_and_missing_ui_runtime_block_completion() {
        let root = tempdir().unwrap();
        let mut no_checks = ProjectConfig::default();
        no_checks.completion.required_checks.clear();
        let result = Orchestrator::new(root.path(), no_checks, Arc::new(StaticWorker), None)
            .unwrap()
            .run(approved_job(root.path()))
            .await
            .unwrap();
        assert_eq!(result.record.state, JobState::Blocked);

        let mut ui = ProjectConfig::default();
        ui.commands.insert("ui".into(), vec!["true".into()]);
        ui.completion.required_checks = vec!["ui".into()];
        ui.ui.enabled = true;
        let result = Orchestrator::new(root.path(), ui, Arc::new(StaticWorker), None)
            .unwrap()
            .run(approved_job(root.path()))
            .await
            .unwrap();
        assert_eq!(result.record.state, JobState::Blocked);
        assert!(result.record.message.contains("[runtime]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_failure_is_detected_without_disabling_test() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let script = root.path().join("check.sh");
        fs::write(&script, "#!/bin/sh\n[ \"$(cat result.txt)\" = complete ]\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = ProjectConfig::default();
        config
            .commands
            .insert("unit".into(), vec![script.display().to_string()]);
        config.completion.required_checks = vec!["unit".into()];
        config.completion.max_attempts = 3;
        let result = Orchestrator::new(root.path(), config, Arc::new(StaticWorker), None)
            .unwrap()
            .run(approved_job(root.path()))
            .await
            .unwrap();
        assert_eq!(result.record.state, JobState::Blocked);
        assert_eq!(result.record.attempt, 2);
        assert!(result.record.message.contains("same check failure"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_complete_claim_cannot_bypass_failed_tests() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let script = root.path().join("always-fails");
        fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = ProjectConfig::default();
        config
            .commands
            .insert("unit".into(), vec![script.display().to_string()]);
        config.completion.required_checks = vec!["unit".into()];
        config.completion.max_attempts = 1;
        let result = Orchestrator::new(root.path(), config, Arc::new(StaticWorker), None)
            .unwrap()
            .run(approved_job(root.path()))
            .await
            .unwrap();
        assert_ne!(result.record.state, JobState::Complete);
        assert_eq!(result.record.state, JobState::Blocked);
        assert!(result
            .gates()
            .unwrap()
            .iter()
            .any(|gate| gate.name == "unit passed" && gate.status == GateStatus::Failed));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reviewer_rejection_blocks_complete_state() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let script = root.path().join("check.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = ProjectConfig::default();
        config
            .commands
            .insert("unit".into(), vec![script.display().to_string()]);
        config.completion.require_review = true;
        let result = Orchestrator::new(
            root.path(),
            config,
            Arc::new(StaticWorker),
            Some(Arc::new(RejectingReviewer)),
        )
        .unwrap()
        .run(approved_job(root.path()))
        .await
        .unwrap();
        assert_eq!(result.record.state, JobState::Blocked);
        assert!(result.record.message.contains("ChangesRequired"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deterministic_subprocess_repair_lifecycle() {
        use std::{net::TcpListener, os::unix::fs::PermissionsExt};

        fn executable(path: &Path, contents: &str) {
            fs::write(path, contents).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let root = tempdir().unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        executable(
            &root.path().join("fake-codex"),
            r#"#!/bin/sh
set -eu
if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then echo "codex-cli fixture"; exit 0; fi
if [ "$#" -eq 1 ] && [ "$1" = "--help" ]; then echo "exec --ask-for-approval never --sandbox read-only workspace-write --cd"; exit 0; fi
if [ "$#" -eq 2 ] && [ "$1" = "exec" ] && [ "$2" = "--help" ]; then echo "Run Codex non-interactively: resume --json --output-schema --output-last-message"; exit 0; fi
if [ "$#" -eq 3 ] && [ "$1" = "exec" ] && [ "$2" = "resume" ] && [ "$3" = "--help" ]; then echo "SESSION_ID --json"; exit 0; fi
last=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-last-message) last=$2; shift 2 ;;
    *) shift ;;
  esac
done
count=0
[ ! -f codex-attempt ] || count=$(cat codex-attempt)
count=$((count + 1))
printf '%s' "$count" > codex-attempt
case "$count" in
  1) printf '%s' incomplete > result.txt ;;
  2) printf '%s' unit-ok > result.txt ;;
  *) printf '%s' complete > result.txt; : > ui-fixed; rm -f test-results/browser-events.jsonl ;;
esac
echo '{"type":"thread.started","thread_id":"fixture-session"}'
echo '{"type":"turn.started"}'
printf '%s' '{"summary":"fake Codex repair","changed_files":["result.txt"],"tests":[]}' > "$last"
"#,
        );
        executable(
            &root.path().join("unit-check"),
            "#!/bin/sh\nset -eu\n[ \"$(cat result.txt)\" != incomplete ]\n",
        );
        executable(
            &root.path().join("ui-check"),
            r#"#!/bin/sh
set -eu
mkdir -p test-results
if [ "$(cat result.txt)" = incomplete ]; then
  echo '{"suites":[{"title":"fixture","specs":[{"title":"button works","file":"fixture.spec.ts","line":12,"tests":[{"status":"passed"}]}]}]}'
  exit 0
fi
if [ ! -f ui-fixed ]; then
  echo '{"kind":"console_error","message":"deliberate first-attempt UI bug","location":"app.js:4"}' > test-results/browser-events.jsonl
  printf screenshot > test-results/failure.png
  printf trace > test-results/trace.zip
  echo '{"suites":[{"title":"fixture","specs":[{"title":"button works","file":"fixture.spec.ts","line":12,"tests":[{"status":"unexpected"}]}]}]}'
  exit 1
fi
echo '{"suites":[{"title":"fixture","specs":[{"title":"button works","file":"fixture.spec.ts","line":12,"tests":[{"status":"passed"}]}]}]}'
"#,
        );
        fs::write(
            root.path().join("server.mjs"),
            format!(
                "import http from 'node:http';\nhttp.createServer((_req,res) => {{ res.writeHead(200); res.end('ok'); }}).listen({port}, '127.0.0.1');\n"
            ),
        )
        .unwrap();

        let store = JobStore::new(root.path()).unwrap();
        let mut job = store
            .create(
                "Fix the fixture behavior",
                "web",
                Some("shared-chat".into()),
            )
            .unwrap();
        job.set_specification(
            "Repair the unit behavior, then repair the browser behavior.",
            &[
                AcceptanceCriterion::new("AC-1", "Unit behavior passes", ["unit_test"]),
                AcceptanceCriterion::new("AC-2", "Browser behavior passes", ["browser_test"]),
                AcceptanceCriterion::new(
                    "AC-3",
                    "All configured evidence passes",
                    ["required_check"],
                ),
            ],
        )
        .unwrap();
        job.approve().unwrap();

        let mut config = ProjectConfig::default();
        config.commands.insert(
            "unit".into(),
            vec![root.path().join("unit-check").display().to_string()],
        );
        config.commands.insert(
            "ui".into(),
            vec![root.path().join("ui-check").display().to_string()],
        );
        config
            .commands
            .insert("serve".into(), vec!["node".into(), "server.mjs".into()]);
        config.completion.required_checks = vec!["unit".into(), "ui".into()];
        config.completion.max_attempts = 4;
        config.completion.require_review = true;
        config.ui.enabled = true;
        config.ui.test_command = "ui".into();
        config.ui.fail_on_console_error = true;
        config.runtime = Some(crate::project_config::RuntimeConfig {
            start_command: "serve".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            healthcheck_url: Some(format!("http://127.0.0.1:{port}/health")),
            startup_timeout_seconds: 5,
        });
        let worker = Arc::new(CodexCliWorker::new(
            root.path().join("fake-codex").display().to_string(),
        ));
        let completed =
            Orchestrator::new(root.path(), config, worker, Some(Arc::new(FakeReviewer)))
                .unwrap()
                .run(job)
                .await
                .unwrap();

        if completed.record.state != JobState::Complete {
            eprintln!(
                "events:\n{}\nchecks:\n{}\ngates:\n{}",
                fs::read_to_string(completed.path("events.jsonl")).unwrap_or_default(),
                fs::read_to_string(completed.path("check-results.json")).unwrap_or_default(),
                fs::read_to_string(completed.path("completion-gates.json")).unwrap_or_default()
            );
        }
        assert_eq!(
            completed.record.state,
            JobState::Complete,
            "{}",
            completed.record.message
        );
        assert_eq!(completed.record.attempt, 3);
        assert!(completed.all_required_gates_pass().unwrap());
        let checks = fs::read_to_string(completed.path("check-results.json")).unwrap();
        assert!(checks.contains("\"category\": \"unit\""));
        assert!(checks.contains("\"category\": \"ui\""));
        let browser =
            fs::read_to_string(completed.artifact_dir().join("playwright-attempt-2.json")).unwrap();
        assert!(browser.contains("deliberate first-attempt UI bug"));
        assert!(browser.contains("failure.png"));
        assert!(browser.contains("trace.zip"));
        let criteria = completed.criteria().unwrap();
        assert!(criteria
            .iter()
            .all(|criterion| criterion.status == crate::jobs::CriterionStatus::Verified));
        assert_eq!(
            fs::read_to_string(root.path().join("result.txt")).unwrap(),
            "complete"
        );
    }
}
