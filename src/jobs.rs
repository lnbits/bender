use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Received,
    Clarifying,
    AwaitingApproval,
    Approved,
    Working,
    Checking,
    Fixing,
    Reviewing,
    AwaitingActionApproval,
    Complete,
    Blocked,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Blocked | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionGate {
    pub name: String,
    pub required: bool,
    pub status: GateStatus,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub explanation: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub state: JobState,
    pub created_at: u64,
    pub updated_at: u64,
    pub sender: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub approved_at: Option<u64>,
    #[serde(default)]
    pub current_worker: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub interrupted: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEvent {
    pub timestamp: u64,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Job {
    root: PathBuf,
    pub record: JobRecord,
}

#[derive(Debug, Clone)]
pub struct JobStore {
    jobs_root: PathBuf,
}

impl JobStore {
    pub fn new(workspace_root: &Path) -> Result<Self> {
        let jobs_root = workspace_root.join(".bender/jobs");
        fs::create_dir_all(&jobs_root)
            .with_context(|| format!("could not create {}", jobs_root.display()))?;
        Ok(Self { jobs_root })
    }

    pub fn create(
        &self,
        request: &str,
        sender: &str,
        conversation_id: Option<String>,
    ) -> Result<Job> {
        let now = now();
        let id = format!(
            "job-{now}-{}-{}",
            std::process::id(),
            JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let root = self.jobs_root.join(&id);
        fs::create_dir_all(root.join("artifacts"))?;
        let record = JobRecord {
            id,
            state: JobState::Received,
            created_at: now,
            updated_at: now,
            sender: sender.to_string(),
            conversation_id,
            approved_at: None,
            current_worker: None,
            attempt: 0,
            interrupted: false,
            message: "Task received".to_string(),
        };
        let job = Job { root, record };
        atomic_write(&job.path("request.md"), request.as_bytes())?;
        atomic_write(&job.path("specification.md"), b"")?;
        write_json(
            &job.path("acceptance-criteria.json"),
            &Vec::<AcceptanceCriterion>::new(),
        )?;
        write_json(&job.path("state.json"), &job.record)?;
        for file in [
            "conversation.jsonl",
            "events.jsonl",
            "worker-invocations.jsonl",
        ] {
            atomic_write(&job.path(file), b"")?;
        }
        for (file, value) in [
            ("changed-files.json", serde_json::json!([])),
            ("check-results.json", serde_json::json!([])),
            ("review.json", serde_json::json!({"status":"not_run"})),
            ("completion-gates.json", serde_json::json!([])),
        ] {
            write_json(&job.path(file), &value)?;
        }
        atomic_write(&job.path("final-report.md"), b"")?;
        job.event("received", "Task received")?;
        Ok(job)
    }

    pub fn load(&self, id: &str) -> Result<Job> {
        if id.contains('/') || id.contains('\\') || !id.starts_with("job-") {
            anyhow::bail!("invalid job id");
        }
        let root = self.jobs_root.join(id);
        let record = read_json(&root.join("state.json"))?;
        Ok(Job { root, record })
    }

    pub fn list(&self) -> Result<Vec<JobRecord>> {
        let mut records = Vec::<JobRecord>::new();
        for entry in fs::read_dir(&self.jobs_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Ok(record) = read_json(&entry.path().join("state.json")) {
                records.push(record);
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Ok(records)
    }

    pub fn recover_interrupted(&self) -> Result<Vec<String>> {
        let mut recovered = Vec::new();
        for record in self.list()? {
            if matches!(
                record.state,
                JobState::Working | JobState::Checking | JobState::Fixing | JobState::Reviewing
            ) {
                let mut job = self.load(&record.id)?;
                job.record.interrupted = true;
                job.transition(
                    JobState::Blocked,
                    "Bender restarted while work was active; resume or retry is required",
                )?;
                recovered.push(record.id);
            }
        }
        Ok(recovered)
    }

    pub fn latest_awaiting_approval(&self, sender: &str) -> Result<Option<Job>> {
        self.latest_in_state(sender, JobState::AwaitingApproval)
    }

    pub fn latest_in_state(&self, sender: &str, state: JobState) -> Result<Option<Job>> {
        for record in self.list()? {
            if record.state == state && record.sender == sender {
                return self.load(&record.id).map(Some);
            }
        }
        Ok(None)
    }
}

impl Job {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn artifact_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    pub fn request(&self) -> Result<String> {
        fs::read_to_string(self.path("request.md")).context("could not read job request")
    }

    pub fn specification(&self) -> Result<String> {
        fs::read_to_string(self.path("specification.md"))
            .context("could not read job specification")
    }

    pub fn set_specification(
        &mut self,
        specification: &str,
        criteria: &[AcceptanceCriterion],
    ) -> Result<()> {
        atomic_write(&self.path("specification.md"), specification.as_bytes())?;
        write_json(&self.path("acceptance-criteria.json"), criteria)?;
        self.transition(
            JobState::AwaitingApproval,
            "Specification is ready for approval",
        )
    }

    pub fn approve(&mut self) -> Result<()> {
        if self.record.state != JobState::AwaitingApproval {
            anyhow::bail!("job is not awaiting approval");
        }
        self.record.approved_at = Some(now());
        self.transition(JobState::Approved, "Specification approved")
    }

    pub fn transition(&mut self, state: JobState, message: impl Into<String>) -> Result<()> {
        let message = message.into();
        let retrying = state == JobState::Approved
            && matches!(self.record.state, JobState::Blocked | JobState::Failed);
        if self.record.state.is_terminal() && self.record.state != state && !retrying {
            anyhow::bail!("cannot transition terminal job {}", self.record.id);
        }
        self.record.state = state;
        self.record.updated_at = now();
        self.record.message = message.clone();
        write_json(&self.path("state.json"), &self.record)?;
        self.event("state", &format!("{state:?}: {message}"))
    }

    pub fn retry(&mut self) -> Result<()> {
        if !matches!(self.record.state, JobState::Blocked | JobState::Failed) {
            anyhow::bail!("only blocked or failed jobs can be retried");
        }
        self.record.interrupted = false;
        self.transition(JobState::Approved, "Job approved for retry")
    }

    pub fn event(&self, kind: &str, message: &str) -> Result<()> {
        append_jsonl(
            &self.path("events.jsonl"),
            &JobEvent {
                timestamp: now(),
                kind: kind.to_string(),
                message: redact(message),
            },
        )
    }

    pub fn set_gates(&self, gates: &[CompletionGate]) -> Result<()> {
        write_json(&self.path("completion-gates.json"), gates)
    }

    pub fn criteria(&self) -> Result<Vec<AcceptanceCriterion>> {
        read_json(&self.path("acceptance-criteria.json"))
    }

    pub fn mark_criteria_verified(&self) -> Result<()> {
        let mut criteria = self.criteria()?;
        for criterion in &mut criteria {
            criterion.verified = true;
        }
        write_json(&self.path("acceptance-criteria.json"), &criteria)
    }

    pub fn gates(&self) -> Result<Vec<CompletionGate>> {
        read_json(&self.path("completion-gates.json"))
    }

    pub fn all_required_gates_pass(&self) -> Result<bool> {
        Ok(self
            .gates()?
            .iter()
            .filter(|gate| gate.required)
            .all(|gate| gate.status == GateStatus::Passed))
    }

    pub fn finish(&mut self, report: &str) -> Result<()> {
        if !self.all_required_gates_pass()? {
            anyhow::bail!("required completion gates have not all passed");
        }
        atomic_write(&self.path("final-report.md"), report.as_bytes())?;
        self.transition(JobState::Complete, "All required completion gates passed")
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic write target has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("atomic write target has invalid name")?;
    let temp = parent.join(format!(".{name}.tmp-{}-{}", std::process::id(), now()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("could not create {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path).with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

pub fn write_json(path: &Path, value: &(impl Serialize + ?Sized)) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &bytes)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))
}

pub fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

pub fn redact(input: &str) -> String {
    let mut output = Vec::new();
    for word in input.split_whitespace() {
        let looks_secret = word.starts_with("sk-")
            || word.starts_with("nsec1")
            || word.starts_with("ghp_")
            || word.starts_with("github_pat_")
            || (word.len() > 24
                && word
                    .split_once('=')
                    .is_some_and(|(key, _)| key.to_ascii_lowercase().contains("token")));
        output.push(if looks_secret { "[REDACTED]" } else { word });
    }
    output.join(" ")
}

pub fn default_gates(required_checks: &[String], require_review: bool) -> Vec<CompletionGate> {
    let mut gates = vec![
        gate("specification approved", true, GateStatus::Passed),
        gate("patch produced", true, GateStatus::NotRun),
        gate("acceptance criteria verified", true, GateStatus::NotRun),
    ];
    gates.extend(
        required_checks
            .iter()
            .map(|name| gate(&format!("{name} passed"), true, GateStatus::NotRun)),
    );
    if require_review {
        gates.push(gate("review approved", true, GateStatus::NotRun));
    }
    gates
}

fn gate(name: &str, required: bool, status: GateStatus) -> CompletionGate {
    CompletionGate {
        name: name.to_string(),
        required,
        status,
        evidence: String::new(),
        explanation: String::new(),
        updated_at: now(),
    }
}

pub fn gate_map(gates: &[CompletionGate]) -> BTreeMap<String, GateStatus> {
    gates
        .iter()
        .map(|gate| (gate.name.clone(), gate.status))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn persists_every_required_job_file_and_recovers_interruption() {
        let root = tempdir().unwrap();
        let store = JobStore::new(root.path()).unwrap();
        let mut job = store.create("do it", "local", None).unwrap();
        for file in [
            "request.md",
            "conversation.jsonl",
            "specification.md",
            "acceptance-criteria.json",
            "state.json",
            "events.jsonl",
            "worker-invocations.jsonl",
            "changed-files.json",
            "check-results.json",
            "review.json",
            "completion-gates.json",
            "final-report.md",
        ] {
            assert!(job.path(file).exists(), "missing {file}");
        }
        job.transition(JobState::Working, "running").unwrap();
        assert_eq!(
            store.recover_interrupted().unwrap(),
            vec![job.record.id.clone()]
        );
        assert_eq!(
            store.load(&job.record.id).unwrap().record.state,
            JobState::Blocked
        );
    }

    #[test]
    fn not_run_is_not_passed() {
        let root = tempdir().unwrap();
        let store = JobStore::new(root.path()).unwrap();
        let job = store.create("do it", "local", None).unwrap();
        job.set_gates(&default_gates(&["unit".into()], false))
            .unwrap();
        assert!(!job.all_required_gates_pass().unwrap());
    }

    #[test]
    fn approval_is_enforced() {
        let root = tempdir().unwrap();
        let store = JobStore::new(root.path()).unwrap();
        let mut job = store.create("do it", "local", None).unwrap();
        assert!(job.approve().is_err());
        job.set_specification(
            "approved spec",
            &[AcceptanceCriterion {
                id: "one".into(),
                description: "do it".into(),
                verified: false,
            }],
        )
        .unwrap();
        job.approve().unwrap();
        assert_eq!(job.record.state, JobState::Approved);
    }
}
