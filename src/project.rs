use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

const MAX_FILE_BYTES: u64 = 24_000;
const MAX_CONTEXT_FILES: usize = 80;

pub fn collect_context(project_root: &Path) -> Result<String> {
    let mut out = String::new();
    out.push_str("Project files:\n");

    let files = project_files(project_root)?;
    for file in &files {
        out.push_str("- ");
        out.push_str(&file.display().to_string());
        out.push('\n');
    }

    out.push_str("\nRelevant file contents:\n");
    for relative in files.into_iter().take(MAX_CONTEXT_FILES) {
        let absolute = project_root.join(&relative);
        let metadata = match absolute.metadata() {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_FILE_BYTES => metadata,
            _ => continue,
        };
        if metadata.len() == 0 {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&absolute) else {
            continue;
        };
        out.push_str("\n--- ");
        out.push_str(&relative.display().to_string());
        out.push_str(" ---\n");
        out.push_str(&raw);
        if !raw.ends_with('\n') {
            out.push('\n');
        }
    }

    Ok(out)
}

fn project_files(project_root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(project_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry.path()))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(project_root)
            .context("walkdir returned path outside project root")?
            .to_path_buf();
        files.push(relative);
    }
    files.sort();
    Ok(files)
}

fn is_ignored(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".git" | ".bender" | "target" | "node_modules" | ".direnv" | ".devenv"
            )
        })
}
