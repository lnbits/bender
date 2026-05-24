use anyhow::{Context, Result};
use reqwest::Client;

use crate::providers::types;

use super::http::{error_for_status_with_body, response_preview};

pub async fn chat(
    url: &str,
    api_key: Option<&str>,
    model: &str,
    prompt: &str,
    action: &str,
) -> Result<types::AgentResponse> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": prompt }
        ],
        "stream": false
    });

    let mut request = Client::new().post(url).json(&body);
    if let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("{action} failed"))?;
    let response = error_for_status_with_body(response, action).await?;

    let value: serde_json::Value = response.json().await.context("invalid chat JSON")?;
    let text = value
        .get("choices")
        .and_then(|value| value.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "chat response did not contain message content. Response preview: {}",
                response_preview(&value)
            )
        })?;
    types::normalize_text(&text)
}
