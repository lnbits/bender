use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};

const WORKER_PROTECTED: &[&str] = &[".git", ".bender"];

#[derive(Debug, Clone)]
pub struct Workspace {
    root: Arc<PathBuf>,
}

impl Workspace {
    pub fn current() -> Result<Self> {
        Self::new(std::env::current_dir().context("could not read current directory")?)
    }

    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().canonicalize().with_context(|| {
            format!(
                "could not canonicalize workspace {}",
                root.as_ref().display()
            )
        })?;
        if !root.is_dir() {
            anyhow::bail!("workspace is not a directory: {}", root.display());
        }
        Ok(Self {
            root: Arc::new(root),
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".bender")
    }

    pub fn initialize(&self) -> Result<()> {
        std::fs::create_dir_all(self.state_dir())
            .with_context(|| format!("could not create {}", self.state_dir().display()))
    }

    pub fn resolve_read(&self, candidate: impl AsRef<Path>) -> Result<PathBuf> {
        let absolute = self.lexical_absolute(candidate.as_ref())?;
        let resolved = absolute
            .canonicalize()
            .with_context(|| format!("could not resolve {}", absolute.display()))?;
        self.require_inside(&resolved)?;
        Ok(resolved)
    }

    pub fn resolve_write(&self, candidate: impl AsRef<Path>) -> Result<PathBuf> {
        let absolute = self.lexical_absolute(candidate.as_ref())?;
        let mut ancestor = absolute.as_path();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .context("write path has no existing ancestor")?;
        }
        let resolved_ancestor = ancestor
            .canonicalize()
            .with_context(|| format!("could not resolve {}", ancestor.display()))?;
        self.require_inside(&resolved_ancestor)?;
        Ok(absolute)
    }

    pub fn resolve_worker_write(&self, candidate: impl AsRef<Path>) -> Result<PathBuf> {
        let absolute = self.resolve_write(candidate.as_ref())?;
        let relative = absolute
            .strip_prefix(self.root())
            .context("worker path escaped workspace")?;
        if relative.components().next().is_some_and(|part| {
            let Component::Normal(name) = part else {
                return false;
            };
            WORKER_PROTECTED.iter().any(|protected| name == *protected)
        }) {
            anyhow::bail!("worker path targets protected Bender or Git state");
        }
        Ok(absolute)
    }

    fn lexical_absolute(&self, candidate: &Path) -> Result<PathBuf> {
        if candidate.as_os_str().is_empty() {
            anyhow::bail!("empty path is not allowed");
        }
        if candidate
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
        {
            anyhow::bail!("parent traversal is not allowed");
        }
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        if !absolute.starts_with(self.root()) {
            anyhow::bail!("path is outside workspace");
        }
        Ok(absolute)
    }

    fn require_inside(&self, resolved: &Path) -> Result<()> {
        if resolved == self.root() || resolved.starts_with(self.root()) {
            Ok(())
        } else {
            anyhow::bail!(
                "resolved path {} is outside workspace {}",
                resolved.display(),
                self.root.display()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn confines_reads_and_writes() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let sibling = parent.path().join("sibling");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(root.join("nested/file"), "ok").unwrap();
        fs::write(sibling.join("secret"), "no").unwrap();
        let workspace = Workspace::new(&root).unwrap();

        assert_eq!(
            workspace.resolve_read("nested/file").unwrap(),
            root.canonicalize().unwrap().join("nested/file")
        );
        assert!(workspace.resolve_write("nested/new/deep/file").is_ok());
        assert!(workspace.resolve_read("../sibling/secret").is_err());
        assert!(workspace.resolve_read(sibling.join("secret")).is_err());
        assert!(workspace.resolve_worker_write(".git/config").is_err());
        assert!(workspace
            .resolve_worker_write(".bender/state.json")
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_direct_and_nested_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let sibling = parent.path().join("sibling");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("secret"), "no").unwrap();
        symlink(&sibling, root.join("escape")).unwrap();
        symlink(&sibling, root.join("nested/escape")).unwrap();
        let workspace = Workspace::new(&root).unwrap();

        assert!(workspace.resolve_read("escape/secret").is_err());
        assert!(workspace.resolve_write("escape/new").is_err());
        assert!(workspace.resolve_read("nested/escape/secret").is_err());
        assert!(workspace.resolve_write("nested/escape/new").is_err());
    }
}
