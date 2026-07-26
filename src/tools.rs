use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::config::Config;

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    pub requires_confirmation: bool,
    pub permissions: Vec<String>,
    pub path: String,
    #[serde(skip)]
    pub root: PathBuf,
    #[serde(skip)]
    pub command: String,
}

#[derive(Debug, Deserialize)]
struct ToolManifest {
    name: String,
    version: Option<String>,
    description: String,
    command: String,
    #[serde(default)]
    requires_confirmation: bool,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub description: String,
    pub permissions: Vec<String>,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub name: String,
    pub output: Value,
}

pub fn discover(config: &Config, project_root: &Path) -> Result<Vec<Tool>> {
    let mut tools = Vec::new();
    let project_root = project_root
        .canonicalize()
        .context("could not canonicalize project root")?;
    for tool_path in config.tool_paths.iter().filter(|path| path.enabled) {
        if !tool_path.path.exists() {
            continue;
        }
        let Ok(canonical_tool_path) = tool_path.path.canonicalize() else {
            continue;
        };
        let allowed_tools_root = project_root.join(".bender").join("tools");
        if !canonical_tool_path.starts_with(&allowed_tools_root) {
            continue;
        }
        if canonical_tool_path.join("bender-tool.toml").exists() {
            if let Ok(tool) = load_tool(&canonical_tool_path) {
                tools.push(tool);
            }
            continue;
        }
        for entry in fs::read_dir(&canonical_tool_path)
            .with_context(|| format!("could not read {}", canonical_tool_path.display()))?
        {
            let entry = entry?;
            let child = entry.path();
            if child.is_dir() && child.join("bender-tool.toml").exists() {
                if let Ok(tool) = load_tool(&child) {
                    tools.push(tool);
                }
            }
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools.dedup_by(|a, b| a.name == b.name);
    Ok(tools)
}

pub fn prompt_section(tools: &[Tool]) -> String {
    if tools.is_empty() {
        return "Available tools: none.\n".to_string();
    }

    let mut out = String::from("Available tools:\n");
    for tool in tools {
        out.push_str(&format!(
            "- {}: {} Requires confirmation: {}. Permissions: {}.\n",
            tool.name,
            tool.description,
            tool.requires_confirmation,
            if tool.permissions.is_empty() {
                "none".to_string()
            } else {
                tool.permissions.join(", ")
            }
        ));
    }
    out.push_str(
        "\nTo request a tool, return JSON with a tool_calls array using only listed tool names. Example: {\"summary\":\"Ready to run the tool.\",\"diff\":\"\",\"tool_calls\":[{\"name\":\"tool-name\",\"input\":{\"key\":\"value\"}}]}\n",
    );
    out
}

pub fn find_tool(tools: &[Tool], name: &str) -> Option<Tool> {
    tools.iter().find(|tool| tool.name == name).cloned()
}

pub async fn execute(tool: &Tool, project_root: &Path, input: &Value) -> Result<ToolResult> {
    let command_path = validate_command(tool)?;
    let mut child = Command::new(command_path)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start tool {}", tool.name))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(serde_json::to_string(input)?.as_bytes())
            .await
            .with_context(|| format!("failed to write input to tool {}", tool.name))?;
        stdin.write_all(b"\n").await?;
    }

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("tool {} did not finish", tool.name))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        anyhow::bail!(
            "tool {} failed: {}",
            tool.name,
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    let value = serde_json::from_str(&stdout)
        .with_context(|| format!("tool {} returned invalid JSON: {}", tool.name, stdout))?;
    Ok(ToolResult {
        name: tool.name.clone(),
        output: value,
    })
}

fn load_tool(root: &Path) -> Result<Tool> {
    let root = root
        .canonicalize()
        .with_context(|| format!("could not canonicalize tool path {}", root.display()))?;
    let manifest_path = root.join("bender-tool.toml");
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("could not read {}", manifest_path.display()))?;
    let manifest: ToolManifest =
        toml::from_str(&raw).with_context(|| format!("invalid {}", manifest_path.display()))?;
    Ok(Tool {
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        requires_confirmation: manifest.requires_confirmation,
        permissions: manifest.permissions,
        path: root.display().to_string(),
        root: root.to_path_buf(),
        command: manifest.command,
    })
}

fn validate_command(tool: &Tool) -> Result<PathBuf> {
    let path = Path::new(&tool.command);
    if path.is_absolute() {
        anyhow::bail!("tool command must be relative");
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        anyhow::bail!("tool command cannot escape its tool folder");
    }
    Ok(tool.root.join(path))
}
