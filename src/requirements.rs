use std::{path::Path, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    jobs::{now, read_json, write_json, AcceptanceCriterion, Job, JobState},
    project_config::ProjectConfig,
    worker::CodexCliWorker,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyingQuestion {
    pub id: String,
    pub question: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarificationAnswer {
    pub question_id: String,
    pub answer: String,
    pub answered_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequirementsDraft {
    pub summary: String,
    pub clarifying_questions: Vec<ClarifyingQuestion>,
    pub proposed_acceptance_criteria: Vec<AcceptanceCriterion>,
    pub risks: Vec<String>,
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequirementsConversation {
    pub draft: RequirementsDraft,
    #[serde(default)]
    pub answers: Vec<ClarificationAnswer>,
    #[serde(default)]
    pub revisions: u32,
}

pub fn draft(request: &str, root: &Path, project: &ProjectConfig) -> RequirementsDraft {
    let normalized = request.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary = sentence_summary(&normalized);
    let lower = normalized.to_ascii_lowercase();
    let mut clarifying_questions = Vec::new();
    if lower.contains("delet")
        && !["immediate", "delay", "soft delete", "recover"]
            .iter()
            .any(|term| lower.contains(term))
    {
        clarifying_questions.push(ClarifyingQuestion {
            id: "Q1".into(),
            question: format!(
                "For “{}”, should deletion be immediate or delayed/recoverable?",
                summary.trim_end_matches('.')
            ),
            reason: "This changes persistence, recovery, and authentication behavior.".into(),
        });
    } else if (lower.starts_with("fix ")
        || lower.starts_with("improve ")
        || lower.starts_with("update "))
        && !lower.contains("when ")
        && !lower.contains("must ")
        && !lower.contains("should ")
    {
        clarifying_questions.push(ClarifyingQuestion {
            id: "Q1".into(),
            question: format!(
                "What observable behavior should prove “{}” is complete?",
                summary.trim_end_matches('.')
            ),
            reason: "The request names a change but does not define a measurable outcome.".into(),
        });
    }

    let required_check = if project
        .completion
        .required_checks
        .iter()
        .any(|check| check == "unit")
    {
        "unit_test"
    } else {
        "required_check"
    };
    let mut criteria = vec![
        AcceptanceCriterion::new(
            "AC-1",
            format!("The requested behavior is implemented: {summary}"),
            [required_check],
        ),
        AcceptanceCriterion::new(
            "AC-2",
            "Every configured required check runs and passes.",
            ["required_check"],
        ),
    ];
    if project.ui.enabled
        || project
            .completion
            .required_checks
            .iter()
            .any(|check| check == &project.ui.test_command)
    {
        criteria.push(AcceptanceCriterion::new(
            "AC-3",
            "The configured browser flow passes without disallowed browser errors.",
            ["browser_test"],
        ));
    }

    let mut risks = Vec::new();
    if lower.contains("auth") || lower.contains("login") || lower.contains("account") {
        risks.push(
            "Authentication and authorization regressions require explicit test evidence.".into(),
        );
    }
    if lower.contains("delete") || lower.contains("migration") || lower.contains("database") {
        risks.push(
            "Persistent data changes may be destructive or require a recovery strategy.".into(),
        );
    }
    if project.ui.enabled {
        risks.push(
            "Browser behavior and console failures must be captured by the configured UI check."
                .into(),
        );
    }
    let mut assumptions = vec![format!(
        "Work is confined to {}.",
        root.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("the current workspace")
    )];
    if !project.completion.required_checks.is_empty() {
        assumptions.push(format!(
            "Approved required checks are: {}.",
            project.completion.required_checks.join(", ")
        ));
    }
    RequirementsDraft {
        summary,
        clarifying_questions,
        proposed_acceptance_criteria: criteria,
        risks,
        assumptions,
    }
}

pub fn start(
    job: &mut Job,
    root: &Path,
    project: &ProjectConfig,
) -> Result<RequirementsConversation> {
    let conversation = RequirementsConversation {
        draft: draft(&job.request()?, root, project),
        answers: Vec::new(),
        revisions: 1,
    };
    write_json(&job.path("requirements.json"), &conversation)?;
    if conversation.draft.clarifying_questions.is_empty() {
        finalize(job, &conversation)?;
    } else {
        job.transition(
            JobState::Clarifying,
            format!(
                "Waiting for answer to {}",
                conversation.draft.clarifying_questions[0].id
            ),
        )?;
    }
    Ok(conversation)
}

pub async fn start_configured(
    job: &mut Job,
    root: &Path,
    project: &ProjectConfig,
    worker: &CodexCliWorker,
) -> Result<RequirementsConversation> {
    if !project.requirements.use_primary_model {
        return start(job, root, project);
    }
    let repository_context = crate::project::collect_context(root)
        .unwrap_or_else(|_| "Repository scan unavailable.".into());
    let prompt = format!(
        "Draft concise, task-specific software requirements in the supplied JSON schema. Ask only materially useful questions that cannot be answered from the repository. Zero questions is valid. Do not expose hidden reasoning. Proposed criteria must name required evidence types such as unit_test, browser_test, lint, build, required_check, or manual.\n\nRequest:\n{}\n\nApproved checks:\n{}\n\nRepository files:\n{}",
        job.request()?,
        project.completion.required_checks.join(", "),
        repository_context
    );
    let raw = worker
        .run_read_only_planner(
            &format!("{}-requirements", job.record.id),
            root,
            &job.artifact_dir(),
            &prompt,
            REQUIREMENTS_SCHEMA,
            Duration::from_secs(300),
        )
        .await?;
    let draft: RequirementsDraft = serde_json::from_str(&raw)?;
    validate_draft(&draft)?;
    persist_and_transition(job, draft)
}

fn persist_and_transition(
    job: &mut Job,
    draft: RequirementsDraft,
) -> Result<RequirementsConversation> {
    let conversation = RequirementsConversation {
        draft,
        answers: Vec::new(),
        revisions: 1,
    };
    write_json(&job.path("requirements.json"), &conversation)?;
    if conversation.draft.clarifying_questions.is_empty() {
        finalize(job, &conversation)?;
    } else {
        job.transition(
            JobState::Clarifying,
            format!(
                "Waiting for answer to {}",
                conversation.draft.clarifying_questions[0].id
            ),
        )?;
    }
    Ok(conversation)
}

pub fn answer(job: &mut Job, answer: &str) -> Result<RequirementsConversation> {
    let mut conversation: RequirementsConversation = read_json(&job.path("requirements.json"))?;
    let next = next_question(&conversation).cloned();
    let Some(question) = next else {
        anyhow::bail!("no unanswered clarification question remains");
    };
    conversation.answers.push(ClarificationAnswer {
        question_id: question.id,
        answer: answer.trim().to_string(),
        answered_at: now(),
    });
    conversation.revisions += 1;
    write_json(&job.path("requirements.json"), &conversation)?;
    if next_question(&conversation).is_none() {
        finalize(job, &conversation)?;
    } else {
        job.transition(
            JobState::Clarifying,
            "Waiting for another clarification answer",
        )?;
    }
    Ok(conversation)
}

pub fn next_question(conversation: &RequirementsConversation) -> Option<&ClarifyingQuestion> {
    conversation
        .draft
        .clarifying_questions
        .iter()
        .find(|question| {
            !conversation
                .answers
                .iter()
                .any(|answer| answer.question_id == question.id)
        })
}

pub fn render_specification(conversation: &RequirementsConversation) -> String {
    let answers = if conversation.answers.is_empty() {
        "No clarification was required.".into()
    } else {
        conversation
            .answers
            .iter()
            .map(|answer| format!("- {}: {}", answer.question_id, answer.answer))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let criteria = conversation
        .draft
        .proposed_acceptance_criteria
        .iter()
        .map(|criterion| {
            format!(
                "- {}: {} (evidence: {})",
                criterion.id,
                criterion.description,
                criterion.required_evidence.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let assumptions = conversation
        .draft
        .assumptions
        .iter()
        .map(|assumption| format!("- {assumption}"))
        .collect::<Vec<_>>()
        .join("\n");
    let risks = conversation
        .draft
        .risks
        .iter()
        .map(|risk| format!("- {risk}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Proposed job specification\n\n## Summary\n\n{}\n\n## Clarification answers\n\n{}\n\n## Acceptance criteria\n\n{}\n\n## Assumptions\n\n{}\n\n## Risks\n\n{}\n",
        conversation.draft.summary,
        answers,
        criteria,
        assumptions,
        if risks.is_empty() {
            "- None identified."
        } else {
            &risks
        }
    )
}

pub fn user_message(job: &Job, conversation: &RequirementsConversation) -> String {
    if let Some(question) = next_question(conversation) {
        return format!(
            "{} — {}\nReason: {}",
            question.id, question.question, question.reason
        );
    }
    let criteria = conversation
        .draft
        .proposed_acceptance_criteria
        .iter()
        .map(|criterion| {
            format!(
                "{}: {} [required evidence: {}]",
                criterion.id,
                criterion.description,
                criterion.required_evidence.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Proposed specification for {}:\n\n{}\n\nReply APPROVE to begin. No worker or project check will run before approval.",
        job.record.id, criteria
    )
}

fn finalize(job: &mut Job, conversation: &RequirementsConversation) -> Result<()> {
    job.set_specification(
        &render_specification(conversation),
        &conversation.draft.proposed_acceptance_criteria,
    )
}

fn sentence_summary(request: &str) -> String {
    let trimmed = request.trim().trim_end_matches(['.', '!', '?']);
    if trimmed.is_empty() {
        return "Complete the requested change.".into();
    }
    let shortened = trimmed.chars().take(160).collect::<String>();
    format!("{shortened}.")
}

fn validate_draft(draft: &RequirementsDraft) -> Result<()> {
    if draft.summary.trim().is_empty() || draft.proposed_acceptance_criteria.is_empty() {
        anyhow::bail!("requirements planner returned an empty summary or no criteria");
    }
    for criterion in &draft.proposed_acceptance_criteria {
        if criterion.id.trim().is_empty()
            || criterion.description.trim().is_empty()
            || criterion.required_evidence.is_empty()
        {
            anyhow::bail!("requirements planner returned an incomplete acceptance criterion");
        }
    }
    Ok(())
}

const REQUIREMENTS_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["summary", "clarifying_questions", "proposed_acceptance_criteria", "risks", "assumptions"],
  "properties": {
    "summary": {"type": "string"},
    "clarifying_questions": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "question", "reason"],
        "properties": {
          "id": {"type": "string"},
          "question": {"type": "string"},
          "reason": {"type": "string"}
        }
      }
    },
    "proposed_acceptance_criteria": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "description", "required_evidence", "evidence", "status", "reviewer_findings"],
        "properties": {
          "id": {"type": "string"},
          "description": {"type": "string"},
          "required_evidence": {"type": "array", "items": {"type": "string"}},
          "evidence": {"type": "array", "maxItems": 0},
          "status": {"type": "string", "enum": ["unverified"]},
          "reviewer_findings": {"type": "array", "maxItems": 0}
        }
      }
    },
    "risks": {"type": "array", "items": {"type": "string"}},
    "assumptions": {"type": "array", "items": {"type": "string"}}
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobStore;
    use tempfile::tempdir;

    #[test]
    fn task_specific_multi_turn_state_is_persisted_and_regenerated() {
        let root = tempdir().unwrap();
        let mut project = ProjectConfig::default();
        project.commands.insert("unit".into(), vec!["true".into()]);
        let store = JobStore::new(root.path()).unwrap();
        let mut job = store
            .create("Add account deletion", "web", Some("shared-chat".into()))
            .unwrap();
        let draft = start(&mut job, root.path(), &project).unwrap();
        assert_eq!(job.record.state, JobState::Clarifying);
        assert!(draft.draft.clarifying_questions[0]
            .question
            .contains("deletion"));
        let updated = answer(&mut job, "Delay deletion for 30 days.").unwrap();
        assert_eq!(job.record.state, JobState::AwaitingApproval);
        assert_eq!(updated.answers[0].question_id, "Q1");
        assert!(job.specification().unwrap().contains("30 days"));
        let persisted: RequirementsConversation =
            read_json(&job.path("requirements.json")).unwrap();
        assert_eq!(persisted.revisions, 2);
    }

    #[test]
    fn clear_task_can_have_zero_questions() {
        let project = ProjectConfig::default();
        let root = tempdir().unwrap();
        let draft = draft(
            "Add a unit test proving empty input returns HTTP 400.",
            root.path(),
            &project,
        );
        assert!(draft.clarifying_questions.is_empty());
    }
}
