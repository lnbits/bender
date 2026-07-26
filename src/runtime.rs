use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    task::JoinHandle,
};

use crate::{jobs::atomic_write, project_config::validate_argv};

pub struct RuntimeProcess {
    child: Child,
    pid: u32,
    stdout: JoinHandle<Result<Vec<u8>>>,
    stderr: JoinHandle<Result<Vec<u8>>>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl RuntimeProcess {
    pub async fn start(
        argv: &[String],
        cwd: &Path,
        artifact_dir: &Path,
        readiness_url: Option<&str>,
        startup_timeout: Duration,
    ) -> Result<Self> {
        validate_argv(argv)?;
        let workspace = crate::workspace::Workspace::new(cwd)?;
        for argument in argv {
            let path = Path::new(argument);
            if path.is_absolute() {
                workspace.resolve_write(path).with_context(|| {
                    format!("absolute runtime argument is outside workspace: {argument}")
                })?;
            }
        }
        std::fs::create_dir_all(artifact_dir)?;
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("DOCKER_HOST")
            .kill_on_drop(true);
        configure_process_group(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("could not start runtime {}", argv.join(" ")))?;
        let pid = child.id().context("runtime did not have a PID")?;
        let stdout = child.stdout.take().context("runtime stdout unavailable")?;
        let stderr = child.stderr.take().context("runtime stderr unavailable")?;
        let stdout_task = tokio::spawn(read_all(stdout));
        let stderr_task = tokio::spawn(read_all(stderr));
        let mut runtime = Self {
            child,
            pid,
            stdout: stdout_task,
            stderr: stderr_task,
            stdout_path: artifact_dir.join("runtime-stdout.log"),
            stderr_path: artifact_dir.join("runtime-stderr.log"),
        };
        if let Err(error) = runtime.wait_ready(readiness_url, startup_timeout).await {
            let _ = runtime.stop().await;
            return Err(error);
        }
        Ok(runtime)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    async fn wait_ready(
        &mut self,
        readiness_url: Option<&str>,
        startup_timeout: Duration,
    ) -> Result<()> {
        let started = tokio::time::Instant::now();
        let client = reqwest::Client::new();
        loop {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!("runtime exited before readiness with {status}");
            }
            let ready = match readiness_url {
                Some(url) => client
                    .get(url)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success()),
                None => started.elapsed() >= Duration::from_millis(150),
            };
            if ready {
                return Ok(());
            }
            if started.elapsed() >= startup_timeout {
                anyhow::bail!("runtime readiness timed out after {startup_timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn stop(mut self) -> Result<()> {
        terminate_process_group(self.pid).await;
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            force_kill_process_group(self.pid).await;
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        let stdout = self
            .stdout
            .await
            .context("runtime stdout reader panicked")??;
        let stderr = self
            .stderr
            .await
            .context("runtime stderr reader panicked")??;
        atomic_write(&self.stdout_path, &stdout)?;
        atomic_write(&self.stderr_path, &stderr)?;
        Ok(())
    }
}

async fn read_all(mut reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    reader.read_to_end(&mut output).await?;
    const LIMIT: usize = 4 * 1024 * 1024;
    if output.len() > LIMIT {
        output.truncate(LIMIT);
    }
    Ok(output)
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
    let _ = Command::new("kill")
        .args(["-TERM", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(unix))]
async fn terminate_process_group(_pid: u32) {}

#[cfg(unix)]
async fn force_kill_process_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(unix))]
async fn force_kill_process_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_process_and_children_are_cleaned_up() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let script = root.path().join("runtime.sh");
        fs::write(&script, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = RuntimeProcess::start(
            &[script.display().to_string()],
            root.path(),
            root.path(),
            None,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let pid = runtime.pid();
        runtime.stop().await.unwrap();
        let status = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!status.success(), "runtime PID remained alive");
        assert!(root.path().join("runtime-stdout.log").exists());
        assert!(root.path().join("runtime-stderr.log").exists());
    }
}
