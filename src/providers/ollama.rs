use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::{config::Config, providers::types};

use super::http::{error_for_status_with_body, response_preview, trim_base_url};

pub async fn list_models(config: &Config) -> Result<Vec<String>> {
    #[derive(Debug, Deserialize)]
    struct OllamaTags {
        models: Vec<OllamaModel>,
    }

    #[derive(Debug, Deserialize)]
    struct OllamaModel {
        name: String,
    }

    let response = Client::new()
        .get(format!(
            "{}/api/tags",
            trim_base_url(&config.ollama_base_url)
        ))
        .send()
        .await
        .context("listing Ollama models failed")?;
    let response = error_for_status_with_body(response, "listing Ollama models").await?;
    let tags: OllamaTags = response.json().await.context("invalid Ollama tags JSON")?;
    let mut ids: Vec<String> = tags.models.into_iter().map(|model| model.name).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub async fn respond(config: &Config, prompt: &str) -> Result<types::AgentResponse> {
    let url = format!("{}/api/chat", trim_base_url(&config.ollama_base_url));
    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            { "role": "user", "content": prompt }
        ],
        "stream": false
    });

    let response = Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .context("Ollama request failed")?;
    let response = error_for_status_with_body(response, "creating Ollama response").await?;

    let value: serde_json::Value = response.json().await.context("invalid Ollama JSON")?;
    let text = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "Ollama response did not contain message content. Response preview: {}",
                response_preview(&value)
            )
        })?;
    types::normalize_text(&text)
}
