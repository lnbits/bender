use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
};

use crate::{jobs, project_config::validate_argv};

const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum EnvironmentPolicy {
    Check,
    Worker,
}

#[derive(Debug, Clone)]
struct ActiveProcess {
    pid: u32,
    cancelled: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub invocation_id: String,
    pub argv: Vec<String>,
    pub working_directory: PathBuf,
    pub pid: u32,
    pub started_at: u64,
    pub finished_at: u64,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
}

impl CommandResult {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }

    pub fn evidence(&self) -> String {
        format!(
            "command: {}\nexit: {:?}\ntimeout: {}\ncancelled: {}\nstdout:\n{}\nstderr:\n{}",
            display_argv(&self.argv),
            self.exit_code,
            self.timed_out,
            self.cancelled,
            self.stdout,
            self.stderr
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandRunner {
    active: Arc<Mutex<HashMap<String, ActiveProcess>>>,
}

impl CommandRunner {
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        invocation_id: impl Into<String>,
        argv: Vec<String>,
        cwd: &Path,
        stdin: Option<&[u8]>,
        timeout: Duration,
        environment: EnvironmentPolicy,
    ) -> Result<CommandResult> {
        validate_argv(&argv)?;
        let invocation_id = invocation_id.into();
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("could not canonicalize {}", cwd.display()))?;
        validate_absolute_arguments(&argv, &cwd)?;
        let started_at = jobs::now();
        let started = Instant::now();
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&cwd)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_environment(&mut command, environment);
        configure_process_group(&mut command);

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {}", display_argv(&argv)))?;
        let pid = child.id().context("child process did not have a PID")?;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active.lock().await.insert(
            invocation_id.clone(),
            ActiveProcess {
                pid,
                cancelled: cancelled.clone(),
            },
        );

        if let Some(input) = stdin {
            if let Some(mut child_stdin) = child.stdin.take() {
                child_stdin.write_all(input).await?;
            }
        }
        let stdout = child
            .stdout
            .take()
            .context("child stdout was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("child stderr was not captured")?;
        let stdout_task = tokio::spawn(read_limited(stdout));
        let stderr_task = tokio::spawn(read_limited(stderr));

        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => (Some(status.context("could not wait for child")?), false),
            Err(_) => {
                terminate_process_group(pid).await;
                let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .ok()
                    .and_then(Result::ok);
                if status.is_none() {
                    force_kill_process_group(pid).await;
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                (status, true)
            }
        };
        self.active.lock().await.remove(&invocation_id);

        let (stdout, stdout_truncated) = stdout_task.await.context("stdout reader failed")??;
        let (stderr, stderr_truncated) = stderr_task.await.context("stderr reader failed")??;
        Ok(CommandResult {
            invocation_id,
            argv,
            working_directory: cwd,
            pid,
            started_at,
            finished_at: jobs::now(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            exit_code: status.and_then(|status| status.code()),
            timed_out,
            cancelled: cancelled.load(Ordering::SeqCst),
            stdout: jobs::redact(&String::from_utf8_lossy(&stdout)),
            stderr: jobs::redact(&String::from_utf8_lossy(&stderr)),
            output_truncated: stdout_truncated || stderr_truncated,
        })
    }

    pub async fn cancel(&self, invocation_id: &str) -> Result<bool> {
        let active = self.active.lock().await.get(invocation_id).cloned();
        let Some(active) = active else {
            return Ok(false);
        };
        active.cancelled.store(true, Ordering::SeqCst);
        terminate_process_group(active.pid).await;
        let processes = self.active.clone();
        let invocation_id = invocation_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let still_active = processes
                .lock()
                .await
                .get(&invocation_id)
                .is_some_and(|process| process.pid == active.pid);
            if still_active {
                force_kill_process_group(active.pid).await;
            }
        });
        Ok(true)
    }
}

fn validate_absolute_arguments(argv: &[String], cwd: &Path) -> Result<()> {
    let workspace = crate::workspace::Workspace::new(cwd)?;
    for argument in argv {
        let path = Path::new(argument);
        if path.is_absolute() {
            workspace.resolve_write(path).with_context(|| {
                format!("absolute command argument is outside workspace: {argument}")
            })?;
        }
    }
    Ok(())
}

async fn read_limited(mut reader: impl tokio::io::AsyncRead + Unpin) -> Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }
    Ok((output, truncated))
}

fn configure_environment(command: &mut Command, policy: EnvironmentPolicy) {
    const COMMON: &[&str] = &[
        "PATH",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTFLAGS",
        "RUST_BACKTRACE",
        "NIX_PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    const WORKER_ONLY: &[&str] = &[
        "HOME",
        "CODEX_HOME",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CODEX_ACCESS_TOKEN",
    ];
    command.env_clear();
    for key in COMMON {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if matches!(policy, EnvironmentPolicy::Worker) {
        for key in WORKER_ONLY {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
    command.env_remove("SSH_AUTH_SOCK");
    command.env_remove("DOCKER_HOST");
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
async fn terminate_process_group(pid: u32) {
    let group = format!("-{pid}");
    let _ = Command::new("kill")
        .args(["-TERM", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(unix))]
async fn terminate_process_group(_pid: u32) {}

#[cfg(unix)]
async fn force_kill_process_group(pid: u32) {
    let group = format!("-{pid}");
    let _ = Command::new("kill")
        .args(["-KILL", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(unix))]
async fn force_kill_process_group(_pid: u32) {}

pub fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=+".contains(ch))
            {
                arg.clone()
            } else {
                format!("{arg:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn records_success_failure_and_timeout() {
        let root = tempdir().unwrap();
        let runner = CommandRunner::default();
        let success = runner
            .run(
                "ok",
                vec!["printf".into(), "ok".into()],
                root.path(),
                None,
                Duration::from_secs(2),
                EnvironmentPolicy::Check,
            )
            .await
            .unwrap();
        assert!(success.success());
        assert_eq!(success.stdout, "ok");

        let timeout = runner
            .run(
                "timeout",
                vec!["sleep".into(), "2".into()],
                root.path(),
                None,
                Duration::from_millis(30),
                EnvironmentPolicy::Check,
            )
            .await
            .unwrap();
        assert!(timeout.timed_out);
        assert!(!timeout.success());
    }

    #[tokio::test]
    async fn cancellation_terminates_process() {
        let root = tempdir().unwrap();
        let runner = CommandRunner::default();
        let task_runner = runner.clone();
        let cwd = root.path().to_path_buf();
        let task = tokio::spawn(async move {
            task_runner
                .run(
                    "cancel-me",
                    vec!["sleep".into(), "30".into()],
                    &cwd,
                    None,
                    Duration::from_secs(40),
                    EnvironmentPolicy::Check,
                )
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(runner.cancel("cancel-me").await.unwrap());
        let result = task.await.unwrap();
        assert!(result.cancelled);
        assert!(!result.success());
    }
}
