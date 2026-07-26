use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    command_runner::{CommandResult, CommandRunner, EnvironmentPolicy},
    jobs::{self, now, write_json, CompletionGate, GateStatus, Job, JobState},
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
                "summary":{"type":"string"}
            },
            "required":["status","summary"]
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
        }
        let raw: RawReview = serde_json::from_str(content)?;
        let status = match raw.status.as_str() {
            "approved" => ReviewStatus::Approved,
            "changes_required" => ReviewStatus::ChangesRequired,
            "blocked" => ReviewStatus::Blocked,
            _ => anyhow::bail!("reviewer returned an unsupported status"),
        };
        Ok(ReviewResult {
            status,
            summary: raw.summary,
        })
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
        job.set_gates(&gates)?;
        let mut session_id = None;
        let mut previous_failure = None::<String>;
        let mut repeated_failures = 0_u32;
        let mut check_history = Vec::<CommandResult>::new();
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
                check_history.push(result);
            }
            if let Some(runtime) = runtime {
                if let Err(error) = runtime.stop().await {
                    failures.push(format!("Application cleanup failed: {error:#}"));
                } else {
                    job.event("runtime", "Application process group stopped")?;
                }
            }
            write_json(&job.path("check-results.json"), &check_history)?;
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

        job.mark_criteria_verified()?;
        update_gate(
            &mut gates,
            "acceptance criteria verified",
            GateStatus::Passed,
            "Configured independent checks passed; any required independent review approved".into(),
        );
        job.set_gates(&gates)?;
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

    fn review_prompt(&self, job: &Job, checks: &[CommandResult]) -> Result<String> {
        Ok(format!(
            "Review this completed software job independently. Return approved, changes_required, or blocked. Do not claim completion; Bender owns completion gates.\n\nRequest:\n{}\n\nSpecification:\n{}\n\nChecks:\n{}",
            job.request()?,
            job.specification()?,
            checks
                .iter()
                .map(CommandResult::evidence)
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
        worker::{CodingWorker, WorkerResult},
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
                summary: "unchanged attempt".into(),
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
            &[AcceptanceCriterion {
                id: "ac-1".into(),
                description: "result.txt contains complete".into(),
                verified: false,
            }],
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
        assert_eq!(completed.record.state, JobState::Complete);
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
            &[AcceptanceCriterion {
                id: "one".into(),
                description: "result is complete".into(),
                verified: false,
            }],
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
}
