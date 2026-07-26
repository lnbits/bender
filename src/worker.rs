use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    codex::CodexCapabilities,
    command_runner::{CommandResult, CommandRunner, EnvironmentPolicy},
    jobs::{self, atomic_write},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub invocation_id: String,
    pub job_id: String,
    pub workspace: PathBuf,
    pub artifacts: PathBuf,
    pub prompt: String,
    pub attempt: u32,
    #[serde(default)]
    pub session_id: Option<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    pub worker: String,
    pub invocation_id: String,
    pub session_id: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    pub process: CommandResult,
}

#[async_trait]
pub trait CodingWorker: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, request: WorkerRequest) -> Result<WorkerResult>;
    async fn cancel(&self, invocation_id: &str) -> Result<bool>;
}

#[derive(Debug, Clone)]
pub struct CodexCliWorker {
    binary: String,
    runner: CommandRunner,
}

impl Default for CodexCliWorker {
    fn default() -> Self {
        Self {
            binary: "codex".to_string(),
            runner: CommandRunner::default(),
        }
    }
}

impl CodexCliWorker {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            runner: CommandRunner::default(),
        }
    }

    fn argv(
        &self,
        workspace: &Path,
        session_id: Option<&str>,
        schema: &Path,
        last_message: &Path,
        capabilities: &CodexCapabilities,
    ) -> Result<Vec<String>> {
        capabilities.ensure_compatible(session_id.is_some())?;
        let mut argv = vec![
            self.binary.clone(),
            "--ask-for-approval".into(),
            "never".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--cd".into(),
            workspace.display().to_string(),
            "exec".into(),
        ];
        if let Some(session_id) = session_id {
            argv.extend(["resume".into(), "--json".into()]);
            argv.extend(["--output-schema".into(), schema.display().to_string()]);
            argv.extend([
                "--output-last-message".into(),
                last_message.display().to_string(),
            ]);
            argv.push(session_id.to_string());
            argv.push("-".into());
        } else {
            argv.extend([
                "--json".into(),
                "--skip-git-repo-check".into(),
                "--output-schema".into(),
                schema.display().to_string(),
                "--output-last-message".into(),
                last_message.display().to_string(),
                "-".into(),
            ]);
        }
        Ok(argv)
    }

    pub fn capabilities(&self, workspace: &Path) -> Result<CodexCapabilities> {
        CodexCapabilities::detect(&self.binary, workspace)
    }

    pub async fn run_read_only_planner(
        &self,
        invocation_id: &str,
        workspace: &Path,
        artifacts: &Path,
        prompt: &str,
        output_schema: &str,
        timeout: Duration,
    ) -> Result<String> {
        std::fs::create_dir_all(artifacts)?;
        let capabilities = self.capabilities(workspace)?;
        capabilities.ensure_planning_compatible()?;
        let schema = artifacts.join(format!("{invocation_id}-requirements-schema.json"));
        let last_message = artifacts.join(format!("{invocation_id}-requirements.json"));
        atomic_write(&schema, output_schema.as_bytes())?;
        let argv = vec![
            self.binary.clone(),
            "--ask-for-approval".into(),
            "never".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--cd".into(),
            workspace.display().to_string(),
            "exec".into(),
            "--ephemeral".into(),
            "--json".into(),
            "--output-schema".into(),
            schema.display().to_string(),
            "--output-last-message".into(),
            last_message.display().to_string(),
            "-".into(),
        ];
        let process = self
            .runner
            .run(
                invocation_id,
                argv,
                workspace,
                Some(prompt.as_bytes()),
                timeout,
                EnvironmentPolicy::Worker,
            )
            .await?;
        atomic_write(
            &artifacts.join(format!("{invocation_id}-planner-stdout.jsonl")),
            process.stdout.as_bytes(),
        )?;
        atomic_write(
            &artifacts.join(format!("{invocation_id}-planner-stderr.log")),
            process.stderr.as_bytes(),
        )?;
        if !process.success() {
            anyhow::bail!(
                "Codex requirements planner failed (exit {:?}, timeout {}, cancelled {}): {}",
                process.exit_code,
                process.timed_out,
                process.cancelled,
                process.stderr
            );
        }
        std::fs::read_to_string(&last_message)
            .context("Codex requirements planner did not produce structured output")
    }
}

#[derive(Debug, Deserialize)]
struct CodexFinal {
    summary: String,
    #[serde(default)]
    changed_files: Vec<String>,
    #[serde(default)]
    tests: Vec<String>,
    #[serde(default)]
    patch: String,
}

#[async_trait]
impl CodingWorker for CodexCliWorker {
    fn name(&self) -> &'static str {
        "codex_cli"
    }

    async fn run(&self, request: WorkerRequest) -> Result<WorkerResult> {
        std::fs::create_dir_all(&request.artifacts)?;
        let schema = request
            .artifacts
            .join(format!("{}-response-schema.json", request.invocation_id));
        let last_message = request
            .artifacts
            .join(format!("{}-last-message.json", request.invocation_id));
        atomic_write(&schema, CODEX_OUTPUT_SCHEMA.as_bytes())?;
        let capabilities = self.capabilities(&request.workspace)?;
        let argv = self.argv(
            &request.workspace,
            request.session_id.as_deref(),
            &schema,
            &last_message,
            &capabilities,
        )?;
        let process = self
            .runner
            .run(
                &request.invocation_id,
                argv,
                &request.workspace,
                Some(request.prompt.as_bytes()),
                Duration::from_secs(request.timeout_seconds),
                EnvironmentPolicy::Worker,
            )
            .await?;

        atomic_write(
            &request
                .artifacts
                .join(format!("{}-stdout.jsonl", request.invocation_id)),
            process.stdout.as_bytes(),
        )?;
        atomic_write(
            &request
                .artifacts
                .join(format!("{}-stderr.log", request.invocation_id)),
            process.stderr.as_bytes(),
        )?;
        if !process.success() {
            anyhow::bail!(
                "Codex invocation failed (exit {:?}, timeout {}, cancelled {}): {}",
                process.exit_code,
                process.timed_out,
                process.cancelled,
                process.stderr
            );
        }
        let raw = std::fs::read_to_string(&last_message)
            .with_context(|| "Codex exited successfully but did not produce structured output")?;
        let final_message: CodexFinal = serde_json::from_str(&raw)
            .with_context(|| "Codex produced malformed structured output")?;
        let session_id = extract_thread_id(&process.stdout).or(request.session_id);
        Ok(WorkerResult {
            worker: self.name().to_string(),
            invocation_id: request.invocation_id,
            session_id,
            summary: final_message.summary,
            changed_files: final_message.changed_files,
            tests: final_message.tests,
            process,
        })
    }

    async fn cancel(&self, invocation_id: &str) -> Result<bool> {
        self.runner.cancel(invocation_id).await
    }
}

#[derive(Debug, Clone)]
pub struct OllamaWorker {
    pub base_url: String,
    pub model: String,
    client: reqwest::Client,
}

impl OllamaWorker {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl CodingWorker for OllamaWorker {
    fn name(&self) -> &'static str {
        "ollama"
    }

    async fn run(&self, request: WorkerRequest) -> Result<WorkerResult> {
        let started = jobs::now();
        let local_prompt = format!(
            "You are Bender's explicitly approved local fallback coding worker. You cannot execute tools. Inspect the supplied task context and return a valid unified diff in the optional `patch` field. Never target .git or .bender. Return only schema-valid JSON.\n\n{}",
            request.prompt
        );
        let value: serde_json::Value = self
            .client
            .post(format!("{}/api/chat", self.base_url.trim_end_matches('/')))
            .json(&serde_json::json!({
                "model": self.model,
                "stream": false,
                "messages": [{"role":"user","content":local_prompt}],
                "format": serde_json::from_str::<serde_json::Value>(CODEX_OUTPUT_SCHEMA)?
            }))
            .send()
            .await
            .context("Ollama worker request failed")?
            .error_for_status()
            .context("Ollama worker returned an error")?
            .json()
            .await
            .context("invalid Ollama worker response")?;
        let text = value
            .pointer("/message/content")
            .and_then(|value| value.as_str())
            .context("Ollama response did not contain message.content")?;
        let mut final_message: CodexFinal =
            serde_json::from_str(text).context("Ollama returned malformed structured output")?;
        if !final_message.patch.trim().is_empty() {
            crate::patch::validate_patch(&request.workspace, &final_message.patch)?;
            crate::patch::store_last_patch(&request.workspace, &final_message.patch)?;
            crate::patch::apply_last_patch(&request.workspace).await?;
            crate::jobs::atomic_write(
                &request
                    .artifacts
                    .join(format!("{}-fallback.patch", request.invocation_id)),
                final_message.patch.as_bytes(),
            )?;
            if final_message.changed_files.is_empty() {
                final_message.changed_files = patch_paths(&final_message.patch);
            }
        }
        let finished = jobs::now();
        Ok(WorkerResult {
            worker: self.name().to_string(),
            invocation_id: request.invocation_id.clone(),
            session_id: None,
            summary: final_message.summary,
            changed_files: final_message.changed_files,
            tests: final_message.tests,
            process: CommandResult {
                invocation_id: request.invocation_id,
                argv: vec!["ollama-api".into(), self.model.clone()],
                working_directory: request.workspace,
                pid: 0,
                started_at: started,
                finished_at: finished,
                elapsed_ms: finished.saturating_sub(started),
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                stdout: jobs::redact(text),
                stderr: String::new(),
                output_truncated: false,
            },
        })
    }

    async fn cancel(&self, _invocation_id: &str) -> Result<bool> {
        Ok(false)
    }
}

pub type SharedWorker = Arc<dyn CodingWorker>;

fn extract_thread_id(jsonl: &str) -> Option<String> {
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|value| value.as_str()) == Some("thread.started") {
            if let Some(id) = value
                .get("thread_id")
                .or_else(|| value.get("threadId"))
                .and_then(|value| value.as_str())
            {
                return Some(id.to_string());
            }
        }
    }
    None
}

const CODEX_OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["summary", "changed_files", "tests"],
  "properties": {
    "summary": {"type": "string"},
    "changed_files": {"type": "array", "items": {"type": "string"}},
    "tests": {"type": "array", "items": {"type": "string"}},
    "patch": {"type": "string"}
  }
}"#;

fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths = patch
        .lines()
        .filter_map(|line| line.strip_prefix("+++ b/"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};
    #[cfg(unix)]
    use tempfile::tempdir;

    #[test]
    fn current_codex_invocation_is_argv_based_and_confined() {
        let worker = CodexCliWorker::default();
        let argv = worker
            .argv(
                Path::new("/tmp/project"),
                None,
                Path::new("/tmp/schema"),
                Path::new("/tmp/result"),
                &CodexCapabilities {
                    version: "fixture".into(),
                    supports_exec: true,
                    supports_json: true,
                    supports_output_schema: true,
                    supports_output_last_message: true,
                    supports_resume: true,
                    supports_workspace_write: true,
                    supports_read_only: true,
                    supports_approval_never: true,
                    supports_working_directory: true,
                },
            )
            .unwrap();
        assert_eq!(argv[0], "codex");
        assert!(argv.windows(2).any(|pair| pair == ["--cd", "/tmp/project"]));
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--sandbox", "workspace-write"]));
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--ask-for-approval", "never"]));
        assert!(argv.contains(&"--json".to_string()));
        assert!(!argv.iter().any(|arg| arg.contains("sh -c")));
    }

    #[test]
    fn extracts_machine_readable_session_id() {
        let id = extract_thread_id(
            "{\"type\":\"thread.started\",\"thread_id\":\"019c-test\"}\n{\"type\":\"turn.started\"}",
        );
        assert_eq!(id.as_deref(), Some("019c-test"));
    }

    #[cfg(unix)]
    fn fake_codex(workspace: &Path, invocation: &str) -> PathBuf {
        let path = workspace.join("fake-codex");
        let script = format!(
            r#"#!/bin/sh
set -eu
if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then
  echo "codex-cli fixture-1.0"
  exit 0
fi
if [ "$#" -eq 1 ] && [ "$1" = "--help" ]; then
  echo "exec --ask-for-approval never --sandbox read-only workspace-write --cd"
  exit 0
fi
if [ "$#" -eq 2 ] && [ "$1" = "exec" ] && [ "$2" = "--help" ]; then
  echo "Run Codex non-interactively: resume --json --output-schema --output-last-message"
  exit 0
fi
if [ "$#" -eq 3 ] && [ "$1" = "exec" ] && [ "$2" = "resume" ] && [ "$3" = "--help" ]; then
  echo "SESSION_ID --json"
  exit 0
fi
printf '%s\n' "$PWD" > observed-cwd
printf '%s\n' "$@" > observed-argv
printf '%s\n' "${{BENDER_SECRET_SENTINEL-unset}}" > observed-environment
last=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-last-message) last=$2; shift 2 ;;
    *) shift ;;
  esac
done
{invocation}
"#
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn fixture_request(workspace: &Path, session_id: Option<&str>, timeout: u64) -> WorkerRequest {
        WorkerRequest {
            invocation_id: "fixture-invocation".into(),
            job_id: "job-fixture".into(),
            workspace: workspace.to_path_buf(),
            artifacts: workspace.join("artifacts"),
            prompt: "fixture prompt".into(),
            attempt: 1,
            session_id: session_id.map(str::to_string),
            timeout_seconds: timeout,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_codex_subprocess_covers_structured_events_artifacts_and_resume() {
        let root = tempdir().unwrap();
        std::env::set_var("BENDER_SECRET_SENTINEL", "must-not-leak");
        let binary = fake_codex(
            root.path(),
            r#"echo '{"type":"thread.started","thread_id":"fixture-session"}'
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"progress"}}'
echo "fixture stderr" >&2
printf '%s' '{"summary":"fixture success","changed_files":["result.txt"],"tests":["unit"]}' > "$last"
exit 0"#,
        );
        let worker = CodexCliWorker::new(binary.display().to_string());
        let first = worker
            .run(fixture_request(root.path(), None, 5))
            .await
            .unwrap();
        assert_eq!(first.session_id.as_deref(), Some("fixture-session"));
        assert_eq!(first.summary, "fixture success");
        assert!(first.process.stdout.contains("turn.started"));
        assert!(first.process.stderr.contains("fixture stderr"));
        assert_eq!(
            fs::read_to_string(root.path().join("observed-cwd"))
                .unwrap()
                .trim(),
            root.path().canonicalize().unwrap().display().to_string()
        );
        assert_eq!(
            fs::read_to_string(root.path().join("observed-environment"))
                .unwrap()
                .trim(),
            "unset"
        );
        let argv = fs::read_to_string(root.path().join("observed-argv")).unwrap();
        assert!(argv.contains("--json"));
        assert!(argv.contains("--output-schema"));
        assert!(argv.contains("--output-last-message"));
        assert!(root
            .path()
            .join("artifacts/fixture-invocation-stdout.jsonl")
            .exists());
        assert!(root
            .path()
            .join("artifacts/fixture-invocation-stderr.log")
            .exists());

        let resumed = worker
            .run(fixture_request(root.path(), first.session_id.as_deref(), 5))
            .await
            .unwrap();
        assert_eq!(resumed.session_id.as_deref(), Some("fixture-session"));
        let argv = fs::read_to_string(root.path().join("observed-argv")).unwrap();
        assert!(argv.lines().any(|argument| argument == "resume"));
        assert!(argv.lines().any(|argument| argument == "fixture-session"));
        std::env::remove_var("BENDER_SECRET_SENTINEL");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_codex_subprocess_rejects_malformed_partial_and_schema_failures() {
        let root = tempdir().unwrap();
        let malformed = fake_codex(
            root.path(),
            r#"echo '{"type":"thread.started","thread_id":"fixture-session"}'
printf '%s' '{malformed' > "$last"
exit 0"#,
        );
        let worker = CodexCliWorker::new(malformed.display().to_string());
        let error = worker
            .run(fixture_request(root.path(), None, 5))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("malformed structured output"));

        let failed = fake_codex(
            root.path(),
            r#"echo '{"type":"thread.started","thread_id":"partial-session"}'
echo "output schema rejected" >&2
exit 42"#,
        );
        let worker = CodexCliWorker::new(failed.display().to_string());
        let error = worker
            .run(fixture_request(root.path(), None, 5))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("exit Some(42)"));
        assert!(error.contains("output schema rejected"));
        assert!(fs::read_to_string(
            root.path()
                .join("artifacts/fixture-invocation-stdout.jsonl")
        )
        .unwrap()
        .contains("partial-session"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_codex_subprocess_timeout_and_cancellation_terminate_the_process_group() {
        let root = tempdir().unwrap();
        let sleeping = fake_codex(root.path(), "sleep 30");
        let worker = CodexCliWorker::new(sleeping.display().to_string());
        let timeout = worker
            .run(fixture_request(root.path(), None, 1))
            .await
            .unwrap_err()
            .to_string();
        assert!(timeout.contains("timeout true"));

        let worker_task = worker.clone();
        let request = fixture_request(root.path(), None, 30);
        let task = tokio::spawn(async move { worker_task.run(request).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(worker.cancel("fixture-invocation").await.unwrap());
        let cancelled = task.await.unwrap().unwrap_err().to_string();
        assert!(cancelled.contains("cancelled true"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn requirements_planner_uses_actual_read_only_codex_subprocess() {
        let root = tempdir().unwrap();
        let binary = fake_codex(
            root.path(),
            r#"echo '{"type":"thread.started","thread_id":"planning-session"}'
printf '%s' '{"summary":"planned"}' > "$last"
exit 0"#,
        );
        let worker = CodexCliWorker::new(binary.display().to_string());
        let raw = worker
            .run_read_only_planner(
                "requirements-fixture",
                root.path(),
                &root.path().join("artifacts"),
                "draft requirements",
                r#"{"type":"object"}"#,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(raw, r#"{"summary":"planned"}"#);
        let argv = fs::read_to_string(root.path().join("observed-argv")).unwrap();
        assert!(argv.lines().any(|argument| argument == "read-only"));
        assert!(argv.lines().any(|argument| argument == "--ephemeral"));
        assert!(!argv.lines().any(|argument| argument == "workspace-write"));
        assert!(root
            .path()
            .join("artifacts/requirements-fixture-planner-stdout.jsonl")
            .exists());
    }
}
