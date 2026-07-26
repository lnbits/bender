use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
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
    ) -> Vec<String> {
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
        argv
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
        let argv = self.argv(
            &request.workspace,
            request.session_id.as_deref(),
            &schema,
            &last_message,
        );
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

    #[test]
    fn current_codex_invocation_is_argv_based_and_confined() {
        let worker = CodexCliWorker::default();
        let argv = worker.argv(
            Path::new("/tmp/project"),
            None,
            Path::new("/tmp/schema"),
            Path::new("/tmp/result"),
        );
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
}
