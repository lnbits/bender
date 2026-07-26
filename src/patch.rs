use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result};
use tokio::process::Command;

pub fn last_patch_path(project_root: &Path) -> PathBuf {
    project_root.join(".bender").join("last.patch")
}

pub fn store_last_patch(project_root: &Path, diff: &str) -> Result<()> {
    let dir = project_root.join(".bender");
    fs::create_dir_all(&dir)?;
    fs::write(last_patch_path(project_root), diff).context("could not write .bender/last.patch")
}

pub fn validate_patch(project_root: &Path, diff: &str) -> Result<()> {
    if diff.trim().is_empty() {
        anyhow::bail!("empty patch");
    }
    if !is_patch(diff) {
        anyhow::bail!("diff did not contain a valid patch header");
    }

    let root = project_root
        .canonicalize()
        .context("could not canonicalize project root")?;

    for raw_path in diff_paths(diff) {
        validate_relative_patch_path(&root, &raw_path)
            .with_context(|| format!("unsafe patch path: {raw_path}"))?;
    }

    Ok(())
}

pub fn is_patch(diff: &str) -> bool {
    diff.lines().any(|line| {
        line.starts_with("diff --git ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("@@ ")
    })
}

pub async fn apply_last_patch(project_root: &Path) -> Result<String> {
    let patch_path = last_patch_path(project_root);
    let diff = fs::read_to_string(&patch_path)
        .with_context(|| format!("could not read {}", patch_path.display()))?;
    validate_patch(project_root, &diff)?;

    run_git(
        project_root,
        &["apply", "--check", patch_path.to_str().unwrap()],
    )
    .await?;
    run_git(project_root, &["apply", patch_path.to_str().unwrap()]).await
}

async fn run_git(project_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run git")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("git {} failed:\n{}{}", args.join(" "), stdout, stderr);
    }

    Ok(format!("{}{}", stdout, stderr))
}

fn diff_paths(diff: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            for part in rest.split_whitespace().take(2) {
                push_git_path(&mut paths, part);
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            push_git_path(&mut paths, path);
        } else if let Some(path) = line.strip_prefix("+++ ") {
            push_git_path(&mut paths, path);
        }
    }
    paths
}

fn push_git_path(paths: &mut Vec<String>, path: &str) {
    let path = path.split('\t').next().unwrap_or(path);
    if path == "/dev/null" {
        return;
    }
    let stripped = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    paths.push(stripped.to_string());
}

fn validate_relative_patch_path(root: &Path, raw_path: &str) -> Result<()> {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        anyhow::bail!("absolute paths are not allowed");
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        anyhow::bail!("parent directory escapes are not allowed");
    }
    if path
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .is_some_and(|first| matches!(first, ".git" | ".bender" | "target" | "node_modules"))
    {
        anyhow::bail!("protected project metadata path");
    }

    crate::workspace::Workspace::new(root)?
        .resolve_worker_write(path)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn patch(path: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+ok\n"
        )
    }

    #[test]
    fn accepts_nested_nonexistent_targets_and_rejects_protected_paths() {
        let root = tempdir().unwrap();
        assert!(validate_patch(root.path(), &patch("src/new/deep/file.rs")).is_ok());
        assert!(validate_patch(root.path(), &patch("../sibling")).is_err());
        assert!(validate_patch(root.path(), &patch(".git/config")).is_err());
        assert!(validate_patch(root.path(), &patch(".bender/config.toml")).is_err());
        assert!(validate_patch(root.path(), &patch("target/output")).is_err());
        assert!(validate_patch(root.path(), &patch("node_modules/pkg")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_patch_through_nested_symlink() {
        use std::os::unix::fs::symlink;
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let sibling = parent.path().join("sibling");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        symlink(&sibling, root.join("nested/escape")).unwrap();
        assert!(validate_patch(&root, &patch("nested/escape/file")).is_err());
    }
}
