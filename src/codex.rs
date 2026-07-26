use std::{path::Path, process::Command};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexCapabilities {
    pub version: String,
    pub supports_exec: bool,
    pub supports_json: bool,
    pub supports_output_schema: bool,
    pub supports_output_last_message: bool,
    pub supports_resume: bool,
    pub supports_workspace_write: bool,
    pub supports_read_only: bool,
    pub supports_approval_never: bool,
    pub supports_working_directory: bool,
}

impl CodexCapabilities {
    pub fn detect(binary: &str, cwd: &Path) -> Result<Self> {
        let version = checked_output(binary, &["--version"], cwd)
            .with_context(|| format!("could not read `{binary} --version`"))?;
        let global_help = checked_output(binary, &["--help"], cwd)
            .with_context(|| format!("could not inspect `{binary} --help`"))?;
        let exec_help = checked_output(binary, &["exec", "--help"], cwd)
            .with_context(|| format!("could not inspect `{binary} exec --help`"))?;
        let resume_help = checked_output(binary, &["exec", "resume", "--help"], cwd).ok();
        Ok(Self {
            version: version.trim().to_string(),
            supports_exec: global_help.contains("exec") && exec_help.contains("non-interactively"),
            supports_json: exec_help.contains("--json"),
            supports_output_schema: exec_help.contains("--output-schema"),
            supports_output_last_message: exec_help.contains("--output-last-message"),
            supports_resume: resume_help
                .as_ref()
                .is_some_and(|help| help.contains("SESSION_ID") && help.contains("--json")),
            supports_workspace_write: global_help.contains("workspace-write"),
            supports_read_only: global_help.contains("read-only"),
            supports_approval_never: global_help.contains("--ask-for-approval")
                && global_help.contains("never"),
            supports_working_directory: global_help.contains("--cd"),
        })
    }

    pub fn missing_required(&self, resume_required: bool) -> Vec<&'static str> {
        let mut missing = Vec::new();
        for (supported, name) in [
            (self.supports_exec, "non-interactive `codex exec`"),
            (self.supports_json, "structured `--json` events"),
            (self.supports_output_schema, "`--output-schema`"),
            (self.supports_output_last_message, "`--output-last-message`"),
            (self.supports_workspace_write, "`--sandbox workspace-write`"),
            (self.supports_approval_never, "`--ask-for-approval never`"),
            (
                self.supports_working_directory,
                "workspace selection with `--cd`",
            ),
        ] {
            if !supported {
                missing.push(name);
            }
        }
        if resume_required && !self.supports_resume {
            missing.push("session resumption with `codex exec resume`");
        }
        missing
    }

    pub fn ensure_compatible(&self, resume_required: bool) -> Result<()> {
        let missing = self.missing_required(resume_required);
        if !missing.is_empty() {
            anyhow::bail!(
                "incompatible Codex CLI {}: unsupported capability: {}",
                self.version,
                missing.join(", ")
            );
        }
        Ok(())
    }

    pub fn ensure_planning_compatible(&self) -> Result<()> {
        let mut missing = Vec::new();
        for (supported, name) in [
            (self.supports_exec, "non-interactive `codex exec`"),
            (self.supports_json, "structured `--json` events"),
            (self.supports_output_schema, "`--output-schema`"),
            (self.supports_output_last_message, "`--output-last-message`"),
            (self.supports_read_only, "`--sandbox read-only`"),
            (self.supports_approval_never, "`--ask-for-approval never`"),
            (
                self.supports_working_directory,
                "workspace selection with `--cd`",
            ),
        ] {
            if !supported {
                missing.push(name);
            }
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "incompatible Codex CLI {} for requirements planning: unsupported capability: {}",
                self.version,
                missing.join(", ")
            );
        }
        Ok(())
    }
}

pub fn authentication_status(binary: &str, cwd: &Path) -> Result<String> {
    checked_output(binary, &["login", "status"], cwd).map(|value| crate::jobs::redact(&value))
}

fn checked_output(binary: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to start {binary}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        anyhow::bail!("{}", crate::jobs::redact(combined.trim()));
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_capability_is_reported_exactly() {
        let capabilities = CodexCapabilities {
            version: "codex-cli fixture".into(),
            supports_exec: true,
            supports_json: true,
            supports_output_schema: false,
            supports_output_last_message: true,
            supports_resume: false,
            supports_workspace_write: true,
            supports_read_only: true,
            supports_approval_never: true,
            supports_working_directory: true,
        };
        let error = capabilities
            .ensure_compatible(true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`--output-schema`"));
        assert!(error.contains("session resumption"));
    }
}
