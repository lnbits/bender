use std::sync::Arc;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;

use crate::{
    chats,
    config::{
        Config, BENDER_BIO, BENDER_NAME, BENDER_PROFILE_BANNER_URL, BENDER_PROFILE_PICTURE_URL,
    },
    jobs::{append_jsonl, AcceptanceCriterion, JobState, JobStore},
    orchestrator::{GemmaReviewer, Orchestrator, SharedReviewer},
    project_config::ProjectConfig,
    web::AppState,
};

pub async fn run(state: AppState) -> Result<()> {
    let config = state.config.lock().await.clone();
    let keys = config.keys()?;
    let client = Client::new(keys.clone());
    for relay in &config.relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;
    let filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(keys.public_key())
        .limit(0);
    let subscription = client.subscribe(filter, None).await?;
    tracing::info!(
        npub = %config.public_key,
        relays = config.relays.len(),
        "nostr listener connected"
    );

    let result = client
        .handle_notifications(|notification| {
            let client = client.clone();
            let state = state.clone();
            async move {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if event.kind != Kind::GiftWrap {
                        return Ok(false);
                    }
                    let Ok(UnwrappedGift { rumor, sender }) = client.unwrap_gift_wrap(&event).await
                    else {
                        tracing::debug!(event_id = %event.id, "could not unwrap gift wrap");
                        return Ok(false);
                    };
                    let current_config = state.config.lock().await.clone();
                    let controller = match current_config.controller() {
                        Ok(Some(controller)) => controller,
                        Ok(None) => {
                            tracing::debug!(event_id = %event.id, "ignored private message: controller_npub is not configured");
                            return Ok(false);
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "ignored private message: invalid controller_npub");
                            return Ok(false);
                        }
                    };
                    if !authorized_controller(&current_config, sender, rumor.kind) {
                        tracing::info!(
                            event_id = %event.id,
                            sender = %sender,
                            controller = %controller,
                            kind = %rumor.kind,
                            "ignored private message"
                        );
                        return Ok(false);
                    }

                    tracing::info!(sender = %sender, "received controller private message");
                    let sender_identity = current_config
                        .controller_npub
                        .clone()
                        .unwrap_or_else(|| sender.to_string());
                    let reply = handle_message(&state, &rumor.content, &sender_identity)
                        .await
                        .unwrap_or_else(|err| format!("Bender error: {err}"));
                    if let Err(err) = client.send_private_msg(sender, reply, []).await {
                        tracing::warn!(error = %err, "could not send private reply");
                    }
                }
                Ok(false)
            }
        })
        .await;

    client.unsubscribe(subscription.id()).await;
    result.context("nostr notification handler failed")
}

pub fn authorized_controller(config: &Config, sender: PublicKey, kind: Kind) -> bool {
    kind == Kind::PrivateDirectMessage
        && config
            .controller()
            .ok()
            .flatten()
            .is_some_and(|controller| controller == sender)
}

pub async fn publish_profile(config: &Config) -> Result<()> {
    let keys = config.keys()?;
    let client = Client::new(keys);
    for relay in &config.relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;
    publish_profile_with_client(&client, config).await
}

async fn publish_profile_with_client(client: &Client, _config: &Config) -> Result<()> {
    let picture = Url::parse(BENDER_PROFILE_PICTURE_URL).context("invalid profile picture URL")?;
    let banner = Url::parse(BENDER_PROFILE_BANNER_URL).context("invalid profile banner URL")?;
    let metadata = Metadata::new()
        .name(BENDER_NAME)
        .display_name(BENDER_NAME)
        .about(BENDER_BIO)
        .picture(picture)
        .banner(banner);
    client.set_metadata(&metadata).await?;
    tracing::info!(name = BENDER_NAME, "published nostr profile metadata");
    Ok(())
}

async fn handle_message(state: &AppState, message: &str, sender: &str) -> Result<String> {
    let trimmed = message.trim();
    let mut chat_store = chats::load(&state.project_root)?;
    if trimmed.eq_ignore_ascii_case("/newchat") {
        chats::new_nostr_chat(&mut chat_store);
        chats::save(&state.project_root, &chat_store)?;
        return Ok("Started a new chat.".to_string());
    }
    let chat_id = chats::ensure_nostr_chat(&mut chat_store);
    let store = JobStore::new(&state.project_root)?;
    let reply = if trimmed.eq_ignore_ascii_case("APPROVE") {
        let Some(mut job) = store.latest_awaiting_approval(sender)? else {
            return Ok("There is no job awaiting your approval.".to_string());
        };
        job.approve()?;
        append_jsonl(
            &job.path("conversation.jsonl"),
            &serde_json::json!({
                "timestamp": crate::jobs::now(),
                "conversation_id": chat_id,
                "sender": sender,
                "direction": "inbound",
                "content": "APPROVE"
            }),
        )?;
        let project = ProjectConfig::load(&state.project_root)?;
        let reviewer: Option<SharedReviewer> = project
            .reviewers
            .get("gemma")
            .filter(|settings| settings.enabled)
            .map(|settings| {
                Arc::new(GemmaReviewer::new(&settings.base_url, &settings.model)) as SharedReviewer
            });
        let orchestrator =
            Orchestrator::new(&state.project_root, project, state.worker.clone(), reviewer)?;
        let job = orchestrator.run(job).await?;
        let response = if job.record.state == JobState::Complete {
            format!(
                "✓ Job complete\n\n{}",
                std::fs::read_to_string(job.path("final-report.md"))?
            )
        } else {
            format!(
                "Job {} stopped in {:?}: {}",
                job.record.id, job.record.state, job.record.message
            )
        };
        append_jsonl(
            &job.path("conversation.jsonl"),
            &serde_json::json!({
                "timestamp": crate::jobs::now(),
                "conversation_id": chat_id,
                "sender": "bender",
                "direction": "outbound",
                "content": &response
            }),
        )?;
        response
    } else {
        if let Some(mut job) = store.latest_in_state(sender, JobState::Clarifying)? {
            append_jsonl(
                &job.path("conversation.jsonl"),
                &serde_json::json!({
                    "timestamp": crate::jobs::now(),
                    "conversation_id": chat_id,
                    "sender": sender,
                    "direction": "inbound",
                    "content": trimmed
                }),
            )?;
            let criteria = standard_criteria();
            let specification = format!(
                "# Proposed job specification\n\n## Original request\n\n{}\n\n## Clarification answers\n\n{}\n\n## Acceptance criteria\n\n{}",
                job.request()?,
                trimmed,
                numbered_criteria(&criteria)
            );
            job.set_specification(&specification, &criteria)?;
            let response = format!(
                "Proposed acceptance criteria for {}:\n{}\n\nReply APPROVE to begin. No worker or project check will run before approval.",
                job.record.id,
                numbered_criteria(&criteria)
            );
            append_job_reply(&job, &chat_id, &response)?;
            response
        } else {
            let mut job = store.create(trimmed, sender, Some(chat_id.clone()))?;
            job.transition(
                JobState::Clarifying,
                "Waiting for scope and acceptance clarification",
            )?;
            append_jsonl(
                &job.path("conversation.jsonl"),
                &serde_json::json!({
                    "timestamp": crate::jobs::now(),
                    "conversation_id": chat_id,
                    "sender": sender,
                    "direction": "inbound",
                    "content": trimmed
                }),
            )?;
            let response = format!(
                "Before I begin {}:\n1. What observable behavior proves this is complete?\n2. Are there compatibility, security, or scope constraints?\n3. Which configured checks are required?\n\nReply with the answers; I will persist a specification for approval.",
                job.record.id
            );
            append_job_reply(&job, &chat_id, &response)?;
            response
        }
    };
    chats::append(&mut chat_store, &chat_id, "user", trimmed)?;
    chats::update_title_from_user(&mut chat_store, &chat_id, trimmed);
    chats::append(&mut chat_store, &chat_id, "assistant", &reply)?;
    chats::save(&state.project_root, &chat_store)?;
    Ok(reply)
}

fn standard_criteria() -> Vec<AcceptanceCriterion> {
    vec![
        AcceptanceCriterion {
            id: "implementation".into(),
            description: "The clarified request is implemented within the selected workspace."
                .into(),
            verified: false,
        },
        AcceptanceCriterion {
            id: "tests".into(),
            description: "All configured required checks pass without disabling legitimate tests."
                .into(),
            verified: false,
        },
        AcceptanceCriterion {
            id: "evidence".into(),
            description: "Bender records changed files, worker results, and check evidence.".into(),
            verified: false,
        },
    ]
}

fn numbered_criteria(criteria: &[AcceptanceCriterion]) -> String {
    criteria
        .iter()
        .enumerate()
        .map(|(index, criterion)| format!("{}. {}", index + 1, criterion.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn append_job_reply(job: &crate::jobs::Job, conversation_id: &str, response: &str) -> Result<()> {
    append_jsonl(
        &job.path("conversation.jsonl"),
        &serde_json::json!({
            "timestamp": crate::jobs::now(),
            "conversation_id": conversation_id,
            "sender": "bender",
            "direction": "outbound",
            "content": response
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeNostrTransport {
        accepted: Vec<String>,
    }

    impl FakeNostrTransport {
        fn receive(&mut self, config: &Config, sender: PublicKey, kind: Kind, message: &str) {
            if authorized_controller(config, sender, kind) {
                self.accepted.push(message.to_string());
            }
        }
    }

    #[test]
    fn only_configured_nostr_controller_is_authorized() {
        let controller = Keys::generate();
        let stranger = Keys::generate();
        let mut config = Config::new(Keys::generate()).unwrap();
        config.controller_npub = Some(controller.public_key().to_bech32().unwrap());
        let mut transport = FakeNostrTransport {
            accepted: Vec::new(),
        };
        transport.receive(
            &config,
            controller.public_key(),
            Kind::PrivateDirectMessage,
            "approved task",
        );
        transport.receive(
            &config,
            stranger.public_key(),
            Kind::PrivateDirectMessage,
            "malicious task",
        );
        transport.receive(
            &config,
            controller.public_key(),
            Kind::TextNote,
            "public task",
        );
        assert_eq!(transport.accepted, vec!["approved task"]);
    }
}
