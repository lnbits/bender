use std::{path::Path, process::Command};

use serde::Serialize;

use crate::{
    codex::{authentication_status, CodexCapabilities},
    config::Config,
    project_config::ProjectConfig,
    workspace::Workspace,
};

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub required: bool,
}

pub fn run(workspace: &Workspace, codex_smoke_test: bool) -> Vec<DoctorCheck> {
    run_with_codex(workspace, "codex", codex_smoke_test)
}

fn run_with_codex(
    workspace: &Workspace,
    codex_binary: &str,
    codex_smoke_test: bool,
) -> Vec<DoctorCheck> {
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
    let codex_version = output(codex_binary, &["--version"], workspace.root());
    checks.push(DoctorCheck {
        name: "Codex CLI installed".into(),
        ok: codex_version.is_ok(),
        detail: codex_version
            .as_ref()
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|_| {
                "Codex CLI not found\n\nInstall Codex CLI, then run:\n    codex login\n    bender doctor"
                    .into()
            }),
        required: true,
    });
    if codex_version.is_ok() {
        match CodexCapabilities::detect(codex_binary, workspace.root()) {
            Ok(capabilities) => {
                for (name, ok, detail) in [
                    (
                        "Codex non-interactive invocation",
                        capabilities.supports_exec,
                        "`codex exec`",
                    ),
                    (
                        "Codex workspace-directory support",
                        capabilities.supports_working_directory,
                        "`--cd`",
                    ),
                    (
                        "Codex structured output",
                        capabilities.supports_json
                            && capabilities.supports_output_schema
                            && capabilities.supports_output_last_message,
                        "`--json`, `--output-schema`, and `--output-last-message`",
                    ),
                    (
                        "Codex workspace-write sandbox",
                        capabilities.supports_workspace_write
                            && capabilities.supports_approval_never,
                        "`--sandbox workspace-write --ask-for-approval never`",
                    ),
                    (
                        "Codex session resumption",
                        capabilities.supports_resume,
                        "`codex exec resume`",
                    ),
                ] {
                    checks.push(DoctorCheck {
                        name: name.into(),
                        ok,
                        detail: if ok {
                            format!("supported by {}", capabilities.version)
                        } else {
                            format!(
                                "incompatible {}: unsupported capability {detail}",
                                capabilities.version
                            )
                        },
                        required: true,
                    });
                }
            }
            Err(error) => checks.push(DoctorCheck {
                name: "Codex CLI compatibility".into(),
                ok: false,
                detail: error.to_string(),
                required: true,
            }),
        }
        let auth = authentication_status(codex_binary, workspace.root());
        checks.push(DoctorCheck {
            name: "Codex authenticated".into(),
            ok: auth.is_ok(),
            detail: auth.map(|value| sanitize(&value)).unwrap_or_else(|_| {
                "Codex CLI is not authenticated\n\nRun:\n    codex login".into()
            }),
            required: true,
        });
        if codex_smoke_test {
            let smoke = output(
                codex_binary,
                &[
                    "--ask-for-approval",
                    "never",
                    "--sandbox",
                    "read-only",
                    "--cd",
                    workspace.root().to_str().unwrap_or("."),
                    "exec",
                    "--ephemeral",
                    "--json",
                    "Without using tools, reply with the single word OK.",
                ],
                workspace.root(),
            );
            checks.push(DoctorCheck {
                name: "Codex harmless smoke test".into(),
                ok: smoke.is_ok(),
                detail: smoke
                    .map(|_| "non-interactive read-only invocation succeeded".into())
                    .unwrap_or_else(|error| error),
                required: true,
            });
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_missing_auth_and_exact_incompatible_capability() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        workspace.initialize().unwrap();
        let binary = root.path().join("codex-fixture");
        executable(
            &binary,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "codex-cli fixture"; exit 0; fi
if [ "$1" = "--help" ]; then echo "exec --ask-for-approval never --sandbox read-only workspace-write --cd"; exit 0; fi
if [ "$1" = "exec" ] && [ "$2" = "--help" ]; then echo "Run Codex non-interactively --json --output-last-message"; exit 0; fi
if [ "$1" = "exec" ] && [ "$2" = "resume" ]; then echo "SESSION_ID --json"; exit 0; fi
if [ "$1" = "login" ]; then echo "not logged in" >&2; exit 1; fi
exit 1
"#,
        );
        let checks = run_with_codex(&workspace, binary.to_str().unwrap(), false);
        let schema = checks
            .iter()
            .find(|check| check.name == "Codex structured output")
            .unwrap();
        assert!(!schema.ok);
        assert!(schema.detail.contains("--output-schema"));
        let auth = checks
            .iter()
            .find(|check| check.name == "Codex authenticated")
            .unwrap();
        assert!(!auth.ok);
        assert!(auth.detail.contains("codex login"));

        let missing = run_with_codex(&workspace, "definitely-missing-codex", false);
        assert!(missing
            .iter()
            .find(|check| check.name == "Codex CLI installed")
            .unwrap()
            .detail
            .contains("Codex CLI not found"));
    }
}
