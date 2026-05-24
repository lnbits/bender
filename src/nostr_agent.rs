use anyhow::{Context, Result};
use nostr_sdk::prelude::*;

use crate::{
    chats,
    config::{
        Config, BENDER_BIO, BENDER_NAME, BENDER_PROFILE_BANNER_URL, BENDER_PROFILE_PICTURE_URL,
    },
    patch, providers, tools,
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
                    if rumor.kind != Kind::PrivateDirectMessage || sender != controller {
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
                    let reply = handle_message(&state, &rumor.content)
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

async fn handle_message(state: &AppState, message: &str) -> Result<String> {
    let trimmed = message.trim();
    let mut chat_store = chats::load(&state.project_root)?;
    if trimmed.eq_ignore_ascii_case("/newchat") {
        chats::new_nostr_chat(&mut chat_store);
        chats::save(&state.project_root, &chat_store)?;
        return Ok("Started a new chat.".to_string());
    }
    let chat_id = chats::ensure_nostr_chat(&mut chat_store);
    let conversation = chats::conversation_prompt(&chat_store, &chat_id);
    let config = state.config.lock().await.clone();
    let available_tools = tools::discover(&config, &state.project_root)?;
    let tools_prompt = tools::prompt_section(&available_tools);
    let response = providers::respond(
        &config,
        &state.project_root,
        trimmed,
        &tools_prompt,
        &conversation,
        &[],
    )
    .await?;

    if !patch::is_patch(&response.diff) {
        if !response.tool_calls.is_empty() {
            let reply = format!(
                "{}\n\nThis request needs a local tool approval. Open the web UI to approve and run tools.",
                response.summary
            );
            chats::append(&mut chat_store, &chat_id, "user", trimmed)?;
            chats::update_title_from_user(&mut chat_store, &chat_id, trimmed);
            chats::append(&mut chat_store, &chat_id, "assistant", &reply)?;
            chats::save(&state.project_root, &chat_store)?;
            return Ok(reply);
        }
        chats::append(&mut chat_store, &chat_id, "user", trimmed)?;
        chats::update_title_from_user(&mut chat_store, &chat_id, trimmed);
        chats::append(&mut chat_store, &chat_id, "assistant", &response.summary)?;
        chats::save(&state.project_root, &chat_store)?;
        return Ok(response.summary);
    }

    patch::validate_patch(&state.project_root, &response.diff)?;
    patch::store_last_patch(&state.project_root, &response.diff)?;
    patch::apply_last_patch(&state.project_root).await?;

    let reply = if response.tool_calls.is_empty() {
        format!("{}\n\nDone.", response.summary)
    } else {
        format!(
            "{}\n\nDone applying changes. This request also needs a local tool approval. Open the web UI to approve and run tools.",
            response.summary
        )
    };
    chats::append(&mut chat_store, &chat_id, "user", trimmed)?;
    chats::update_title_from_user(&mut chat_store, &chat_id, trimmed);
    chats::append(&mut chat_store, &chat_id, "assistant", &reply)?;
    chats::save(&state.project_root, &chat_store)?;
    Ok(reply)
}
