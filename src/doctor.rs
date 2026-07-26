use std::{path::Path, process::Command};

use serde::Serialize;

use crate::{config::Config, project_config::ProjectConfig, workspace::Workspace};

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub required: bool,
}

pub fn run(workspace: &Workspace) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    checks.push(DoctorCheck {
        name: "workspace canonicalization".into(),
        ok: workspace.root().is_absolute(),
        detail: workspace.root().display().to_string(),
        required: true,
    });
    checks.push(path_check(
        "writable project .bender directory",
        &workspace.state_dir(),
        true,
    ));

    let config = Config::load(workspace.root());
    checks.push(DoctorCheck {
        name: "Bender configuration".into(),
        ok: config.is_ok(),
        detail: config
            .as_ref()
            .map(|_| Config::path(workspace.root()).display().to_string())
            .unwrap_or_else(|error| error.to_string()),
        required: true,
    });
    checks.push(DoctorCheck {
        name: "Nostr controller configuration".into(),
        ok: config
            .as_ref()
            .ok()
            .and_then(|config| config.controller_npub.as_deref())
            .is_some_and(|controller| !controller.trim().is_empty()),
        detail: if config
            .as_ref()
            .ok()
            .and_then(|config| config.controller_npub.as_deref())
            .is_some_and(|controller| !controller.trim().is_empty())
        {
            "configured".into()
        } else {
            "optional: not configured".into()
        },
        required: false,
    });
    let codex_version = output("codex", &["--version"], workspace.root());
    checks.push(DoctorCheck {
        name: "Codex CLI installed".into(),
        ok: codex_version.is_ok(),
        detail: codex_version
            .as_ref()
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|error| error.clone()),
        required: true,
    });
    checks.push(DoctorCheck {
        name: "Codex CLI version".into(),
        ok: codex_version.is_ok(),
        detail: codex_version.unwrap_or_else(|error| error),
        required: true,
    });
    let auth = output("codex", &["login", "status"], workspace.root());
    checks.push(DoctorCheck {
        name: "Codex authenticated".into(),
        ok: auth.is_ok(),
        detail: auth
            .map(|value| sanitize(&value))
            .unwrap_or_else(|error| error),
        required: true,
    });
    let help = output("codex", &["exec", "--help"], workspace.root());
    checks.push(DoctorCheck {
        name: "non-interactive Codex invocation".into(),
        ok: help
            .as_ref()
            .is_ok_and(|value| value.contains("--json") && value.contains("--output-schema")),
        detail: "requires `codex exec --json --output-schema`".into(),
        required: true,
    });
    checks.push(program_check(
        "Git",
        "git",
        &["--version"],
        workspace.root(),
        true,
    ));

    let project = ProjectConfig::load(workspace.root());
    checks.push(DoctorCheck {
        name: "project commands".into(),
        ok: project
            .as_ref()
            .is_ok_and(|project| !project.commands.is_empty()),
        detail: project
            .as_ref()
            .map(|project| {
                if project.commands.is_empty() {
                    "none approved; run `bender setup`".into()
                } else {
                    project
                        .commands
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            })
            .unwrap_or_else(|error| error.to_string()),
        required: true,
    });
    if let Ok(project) = project {
        let programs: std::collections::BTreeSet<_> = project
            .commands
            .values()
            .filter_map(|argv| argv.first())
            .map(String::as_str)
            .collect();
        for (label, program, relevant) in [
            (
                "Node/npm/npx",
                "node",
                programs
                    .iter()
                    .any(|p| matches!(*p, "node" | "npm" | "npx")),
            ),
            ("Python/uv", "uv", programs.contains("uv")),
            ("Rust/Cargo", "cargo", programs.contains("cargo")),
            (
                "Playwright",
                "npx",
                project.ui.enabled
                    || programs
                        .iter()
                        .any(|program| program.contains("playwright")),
            ),
        ] {
            if relevant {
                checks.push(program_check(
                    label,
                    program,
                    &["--version"],
                    workspace.root(),
                    true,
                ));
            }
        }
        checks.push(DoctorCheck {
            name: "project runtime".into(),
            ok: project.runtime.is_some() || !project.ui.enabled,
            detail: if project.runtime.is_some() {
                "configured".into()
            } else {
                "not configured".into()
            },
            required: project.ui.enabled,
        });
    }
    checks.push(DoctorCheck {
        name: "paths remain within workspace".into(),
        ok: workspace.resolve_write(".bender/doctor-probe").is_ok()
            && workspace.resolve_read(workspace.root()).is_ok()
            && workspace
                .resolve_read(workspace.root().join("../"))
                .is_err(),
        detail: "canonical path guard active".into(),
        required: true,
    });
    checks
}

fn path_check(name: &str, path: &Path, required: bool) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        ok: path.is_dir(),
        detail: path.display().to_string(),
        required,
    }
}

fn program_check(
    name: &str,
    program: &str,
    args: &[&str],
    cwd: &Path,
    required: bool,
) -> DoctorCheck {
    match output(program, args, cwd) {
        Ok(detail) => DoctorCheck {
            name: name.into(),
            ok: true,
            detail: detail.trim().to_string(),
            required,
        },
        Err(detail) => DoctorCheck {
            name: name.into(),
            ok: false,
            detail,
            required,
        },
    }
}

fn output(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| error.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(sanitize(&text))
    }
}

fn sanitize(value: &str) -> String {
    crate::jobs::redact(value.trim())
}
