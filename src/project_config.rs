use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectSection {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub start_command: String,
    pub base_url: String,
    #[serde(default)]
    pub healthcheck_url: Option<String>,
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ui_command")]
    pub test_command: String,
    #[serde(default = "default_browser")]
    pub browser: String,
    #[serde(default = "default_true")]
    pub fail_on_console_error: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            test_command: default_ui_command(),
            browser: default_browser(),
            fail_on_console_error: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_worker_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionPolicy {
    #[serde(default = "default_required_checks")]
    pub required_checks: Vec<String>,
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_true")]
    pub require_approval: bool,
    #[serde(default)]
    pub require_review: bool,
}

impl Default for CompletionPolicy {
    fn default() -> Self {
        Self {
            required_checks: default_required_checks(),
            max_attempts: default_attempts(),
            require_approval: true,
            require_review: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub project: ProjectSection,
    #[serde(default)]
    pub commands: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub runtime: Option<RuntimeConfig>,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub workers: BTreeMap<String, WorkerSettings>,
    #[serde(default)]
    pub reviewers: BTreeMap<String, WorkerSettings>,
    #[serde(default)]
    pub completion: CompletionPolicy,
}

impl ProjectConfig {
    pub fn path(root: &Path) -> PathBuf {
        root.join(".bender/project.toml")
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let config =
            toml::from_str(&raw).with_context(|| format!("could not parse {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        fs::create_dir_all(path.parent().expect("project config has parent"))?;
        let raw = toml::to_string_pretty(self).context("could not serialize project config")?;
        crate::jobs::atomic_write(&path, raw.as_bytes())
    }

    pub fn command(&self, category: &str) -> Result<&[String]> {
        let argv = self
            .commands
            .get(category)
            .with_context(|| format!("no approved `{category}` command in .bender/project.toml"))?;
        validate_argv(argv)?;
        Ok(argv)
    }

    pub fn detected(root: &Path) -> Self {
        let mut config = Self::default();
        config.project.name = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_string();
        if root.join("Cargo.toml").exists() {
            config.commands.insert(
                "build".into(),
                vec!["cargo".into(), "build".into(), "--locked".into()],
            );
            config.commands.insert(
                "unit".into(),
                vec!["cargo".into(), "test".into(), "--locked".into()],
            );
        }
        if root.join("package.json").exists() {
            config.commands.insert(
                "build".into(),
                vec!["npm".into(), "run".into(), "build".into()],
            );
            config
                .commands
                .insert("unit".into(), vec!["npm".into(), "test".into()]);
        }
        if root.join("pyproject.toml").exists() {
            config.commands.insert(
                "unit".into(),
                vec!["uv".into(), "run".into(), "pytest".into(), "-q".into()],
            );
        }
        config
    }
}

pub fn validate_argv(argv: &[String]) -> Result<()> {
    if argv.is_empty() || argv[0].trim().is_empty() {
        anyhow::bail!("approved command must be a non-empty argv array");
    }
    if matches!(argv[0].as_str(), "sudo" | "su") {
        anyhow::bail!("privilege escalation is not allowed");
    }
    if argv[0] == "git" && argv.get(1).is_some_and(|arg| arg == "push") {
        anyhow::bail!("git push requires a separate explicit action approval");
    }
    if argv[0] == "docker" {
        anyhow::bail!("Docker access requires a separate explicit action approval");
    }
    Ok(())
}

fn default_true() -> bool {
    true
}
fn default_startup_timeout() -> u64 {
    90
}
fn default_worker_timeout() -> u64 {
    1800
}
fn default_attempts() -> u32 {
    4
}
fn default_ui_command() -> String {
    "ui".to_string()
}
fn default_browser() -> String {
    "chromium".to_string()
}
fn default_required_checks() -> Vec<String> {
    vec!["unit".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_argv_and_reject_dangerous_programs() {
        assert!(validate_argv(&["cargo".into(), "test".into()]).is_ok());
        assert!(validate_argv(&["sudo".into(), "cargo".into(), "test".into()]).is_err());
        assert!(validate_argv(&["git".into(), "push".into()]).is_err());
        assert!(validate_argv(&[]).is_err());
    }
}
