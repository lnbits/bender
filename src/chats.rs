use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatStore {
    #[serde(default)]
    pub active_web_chat_id: Option<String>,
    #[serde(default)]
    pub active_nostr_chat_id: Option<String>,
    #[serde(default)]
    pub chats: Vec<Chat>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
}

pub fn path(project_root: &Path) -> PathBuf {
    project_root.join(".bender").join("chats.json")
}

pub fn load(project_root: &Path) -> Result<ChatStore> {
    let path = path(project_root);
    if !path.exists() {
        return Ok(ChatStore::default());
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("invalid {}", path.display()))
}

pub fn save(project_root: &Path, store: &ChatStore) -> Result<()> {
    let path = path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_string_pretty(store)?)
        .with_context(|| format!("could not write {}", path.display()))
}

pub fn ensure_web_chat(store: &mut ChatStore) -> String {
    if let Some(id) = store.active_web_chat_id.clone() {
        if store.chats.iter().any(|chat| chat.id == id) {
            return id;
        }
    }
    let id = new_chat(store, "New chat");
    store.active_web_chat_id = Some(id.clone());
    id
}

pub fn ensure_nostr_chat(store: &mut ChatStore) -> String {
    if let Some(id) = store.active_nostr_chat_id.clone() {
        if store.chats.iter().any(|chat| chat.id == id) {
            return id;
        }
    }
    let id = new_chat(store, "Nostr DM");
    store.active_nostr_chat_id = Some(id.clone());
    id
}

pub fn new_web_chat(store: &mut ChatStore) -> String {
    let id = new_chat(store, "New chat");
    store.active_web_chat_id = Some(id.clone());
    id
}

pub fn new_nostr_chat(store: &mut ChatStore) -> String {
    let id = new_chat(store, "Nostr DM");
    store.active_nostr_chat_id = Some(id.clone());
    id
}

pub fn set_active_web(store: &mut ChatStore, id: &str) -> Result<()> {
    if !store.chats.iter().any(|chat| chat.id == id) {
        anyhow::bail!("chat not found");
    }
    store.active_web_chat_id = Some(id.to_string());
    Ok(())
}

pub fn append(store: &mut ChatStore, id: &str, role: &str, content: &str) -> Result<()> {
    let chat = store
        .chats
        .iter_mut()
        .find(|chat| chat.id == id)
        .ok_or_else(|| anyhow::anyhow!("chat not found"))?;
    chat.messages.push(ChatMessage {
        role: role.to_string(),
        content: content.to_string(),
        created_at: now(),
    });
    Ok(())
}

pub fn messages(store: &ChatStore, id: &str) -> Vec<ChatMessage> {
    store
        .chats
        .iter()
        .find(|chat| chat.id == id)
        .map(|chat| chat.messages.clone())
        .unwrap_or_default()
}

pub fn summaries(store: &ChatStore) -> Vec<ChatSummary> {
    let mut summaries: Vec<_> = store
        .chats
        .iter()
        .map(|chat| ChatSummary {
            id: chat.id.clone(),
            title: chat.title.clone(),
            updated_at: chat
                .messages
                .last()
                .map(|message| message.created_at)
                .unwrap_or_else(|| {
                    chat.id
                        .trim_start_matches("chat-")
                        .parse()
                        .unwrap_or_default()
                }),
        })
        .collect();
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
    summaries
}

pub fn conversation_prompt(store: &ChatStore, id: &str) -> String {
    let Some(chat) = store.chats.iter().find(|chat| chat.id == id) else {
        return "No previous messages.".to_string();
    };
    if chat.messages.is_empty() {
        return "No previous messages.".to_string();
    }
    chat.messages
        .iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|message| format!("{}: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn update_title_from_user(store: &mut ChatStore, id: &str, user_message: &str) {
    let Some(chat) = store.chats.iter_mut().find(|chat| chat.id == id) else {
        return;
    };
    if chat.title != "New chat" && chat.title != "Nostr DM" {
        return;
    }
    let title = user_message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        return;
    }
    chat.title = if title.chars().count() > 48 {
        format!("{}...", title.chars().take(48).collect::<String>())
    } else {
        title
    };
}

fn new_chat(store: &mut ChatStore, title: impl Into<String>) -> String {
    let mut id = format!("chat-{}", now());
    let mut suffix = 2;
    while store.chats.iter().any(|chat| chat.id == id) {
        id = format!("chat-{}-{suffix}", now());
        suffix += 1;
    }
    store.chats.push(Chat {
        id: id.clone(),
        title: title.into(),
        messages: Vec::new(),
    });
    id
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
