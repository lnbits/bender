use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{command_runner::CommandResult, jobs::atomic_write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserIssueKind {
    ConsoleError,
    PageException,
    PageCrash,
    FailedRequest,
    AssertionFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserIssue {
    pub kind: BrowserIssueKind,
    pub message: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub ignored: bool,
    #[serde(default)]
    pub ignored_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserTest {
    pub name: String,
    pub file: String,
    pub line: u64,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaywrightEvidence {
    pub command_passed: bool,
    pub suites: Vec<String>,
    pub tests: Vec<BrowserTest>,
    pub issues: Vec<BrowserIssue>,
    pub screenshots: Vec<String>,
    pub traces: Vec<String>,
    pub videos: Vec<String>,
}

impl PlaywrightEvidence {
    pub fn collect(
        process: &CommandResult,
        artifact_dir: &Path,
        ignored_console_patterns: &[String],
    ) -> Result<Self> {
        let mut evidence = Self {
            command_passed: process.success(),
            ..Self::default()
        };
        if let Some(value) = parse_json_report(&process.stdout) {
            collect_suite(&value, &mut evidence);
        }
        collect_browser_events(
            &artifact_dir.join("browser-events.jsonl"),
            ignored_console_patterns,
            &mut evidence,
        )?;
        collect_artifacts(artifact_dir, artifact_dir, &mut evidence)?;
        if !process.success()
            && !evidence
                .issues
                .iter()
                .any(|issue| issue.kind == BrowserIssueKind::AssertionFailure)
        {
            evidence.issues.push(BrowserIssue {
                kind: BrowserIssueKind::AssertionFailure,
                message: if process.stderr.trim().is_empty() {
                    "Playwright command failed".into()
                } else {
                    process.stderr.clone()
                },
                url: String::new(),
                location: String::new(),
                ignored: false,
                ignored_by: None,
            });
        }
        Ok(evidence)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn unignored(&self, kind: BrowserIssueKind) -> Vec<&BrowserIssue> {
        self.issues
            .iter()
            .filter(|issue| issue.kind == kind && !issue.ignored)
            .collect()
    }
}

fn parse_json_report(output: &str) -> Option<Value> {
    output
        .lines()
        .find_map(|line| {
            serde_json::from_str::<Value>(line).ok().filter(|value| {
                value.get("suites").is_some()
                    || value.get("config").is_some()
                    || value.get("stats").is_some()
            })
        })
        .or_else(|| serde_json::from_str(output).ok())
}

fn collect_suite(value: &Value, evidence: &mut PlaywrightEvidence) {
    if let Some(suites) = value.get("suites").and_then(Value::as_array) {
        for suite in suites {
            if let Some(title) = suite.get("title").and_then(Value::as_str) {
                if !title.is_empty() {
                    evidence.suites.push(title.to_string());
                }
            }
            if let Some(specs) = suite.get("specs").and_then(Value::as_array) {
                for spec in specs {
                    let file = spec
                        .get("file")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let line = spec.get("line").and_then(Value::as_u64).unwrap_or_default();
                    let title = spec
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("unnamed Playwright test");
                    if let Some(tests) = spec.get("tests").and_then(Value::as_array) {
                        for test in tests {
                            let status = test
                                .get("status")
                                .and_then(Value::as_str)
                                .or_else(|| {
                                    test.pointer("/results/0/status").and_then(Value::as_str)
                                })
                                .unwrap_or("unknown");
                            evidence.tests.push(BrowserTest {
                                name: title.to_string(),
                                file: file.clone(),
                                line,
                                status: status.to_string(),
                            });
                            if !matches!(status, "expected" | "passed" | "skipped") {
                                evidence.issues.push(BrowserIssue {
                                    kind: BrowserIssueKind::AssertionFailure,
                                    message: format!("{title}: {status}"),
                                    url: String::new(),
                                    location: format!("{file}:{line}"),
                                    ignored: false,
                                    ignored_by: None,
                                });
                            }
                        }
                    }
                }
            }
            collect_suite(suite, evidence);
        }
    }
}

fn collect_browser_events(
    path: &Path,
    ignored_patterns: &[String],
    evidence: &mut PlaywrightEvidence,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("malformed browser event in {}", path.display()))?;
        let kind = match value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "console_error" => BrowserIssueKind::ConsoleError,
            "page_exception" => BrowserIssueKind::PageException,
            "page_crash" => BrowserIssueKind::PageCrash,
            "failed_request" => BrowserIssueKind::FailedRequest,
            "assertion_failure" => BrowserIssueKind::AssertionFailure,
            _ => continue,
        };
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("browser issue")
            .to_string();
        let ignored_by = if kind == BrowserIssueKind::ConsoleError {
            ignored_patterns
                .iter()
                .find(|pattern| wildcard_match(pattern, &message))
                .cloned()
        } else {
            None
        };
        evidence.issues.push(BrowserIssue {
            kind,
            message,
            url: value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            location: value
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ignored: ignored_by.is_some(),
            ignored_by,
        });
    }
    Ok(())
}

fn collect_artifacts(
    root: &Path,
    directory: &Path,
    evidence: &mut PlaywrightEvidence,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_artifacts(root, &path, evidence)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("png") => evidence.screenshots.push(relative),
            Some("webm" | "mp4") => evidence.videos.push(relative),
            Some("zip") if path.file_name().is_some_and(|name| name == "trace.zip") => {
                evidence.traces.push(relative)
            }
            _ => {}
        }
    }
    Ok(())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut matched) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            matched = v;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            matched += 1;
            v = matched;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::now;
    use tempfile::tempdir;

    #[test]
    fn parses_playwright_and_records_ignored_console_evidence() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("browser-events.jsonl"),
            "{\"kind\":\"console_error\",\"message\":\"GET favicon.ico 404\"}\n\
             {\"kind\":\"page_exception\",\"message\":\"boom\",\"location\":\"app.js:3\"}\n",
        )
        .unwrap();
        fs::write(root.path().join("failure.png"), "png").unwrap();
        fs::write(root.path().join("trace.zip"), "trace").unwrap();
        let process = CommandResult {
            invocation_id: "ui".into(),
            argv: vec!["playwright".into(), "test".into()],
            working_directory: root.path().into(),
            pid: 1,
            started_at: now(),
            finished_at: now(),
            elapsed_ms: 1,
            exit_code: Some(1),
            timed_out: false,
            cancelled: false,
            stdout: r#"{"suites":[{"title":"account","specs":[{"title":"deletes","file":"account.spec.ts","line":9,"tests":[{"status":"unexpected"}]}]}]}"#.into(),
            stderr: String::new(),
            output_truncated: false,
        };
        let evidence =
            PlaywrightEvidence::collect(&process, root.path(), &["*favicon.ico*".into()]).unwrap();
        assert_eq!(evidence.tests[0].file, "account.spec.ts");
        assert_eq!(evidence.screenshots, ["failure.png"]);
        assert_eq!(evidence.traces, ["trace.zip"]);
        assert!(evidence.issues.iter().any(|issue| {
            issue.kind == BrowserIssueKind::ConsoleError
                && issue.ignored
                && issue.ignored_by.as_deref() == Some("*favicon.ico*")
        }));
        assert_eq!(evidence.unignored(BrowserIssueKind::PageException).len(), 1);
    }
}
