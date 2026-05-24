use anyhow::{Context, Result};
use reqwest::Client;

use crate::{config::Config, providers::types};

use super::http::{error_for_status_with_body, response_preview};

pub async fn list_models(_config: &Config) -> Result<Vec<String>> {
    Ok(Vec::new())
}

pub async fn respond(
    config: &Config,
    prompt: &str,
    images: &[types::PromptImage],
) -> Result<types::AgentResponse> {
    let mut content = vec![serde_json::json!({
        "type": "text",
        "text": prompt
    })];
    for image in images {
        let Some((_, data)) = image.data_url.split_once(',') else {
            continue;
        };
        content.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": image.media_type,
                "data": data
            }
        }));
    }
    let body = serde_json::json!({
        "model": config.model,
        "max_tokens": 4096,
        "messages": [
            { "role": "user", "content": content }
        ]
    });

    let response = Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", config.api_key()?.unwrap_or_default())
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("Claude request failed")?;
    let response = error_for_status_with_body(response, "creating Claude response").await?;

    let value: serde_json::Value = response.json().await.context("invalid Claude JSON")?;
    let text = extract_text(&value).with_context(|| {
        format!(
            "Claude response did not contain text. Response preview: {}",
            response_preview(&value)
        )
    })?;
    types::normalize_text(&text)
}

fn extract_text(value: &serde_json::Value) -> Option<String> {
    let mut out = String::new();
    let content = value.get("content")?.as_array()?;
    for item in content {
        let is_text = item
            .get("type")
            .and_then(|value| value.as_str())
            .is_some_and(|kind| kind == "text");
        if is_text {
            if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                out.push_str(text);
            }
        }
    }
    (!out.trim().is_empty()).then_some(out)
}
