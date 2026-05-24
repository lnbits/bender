use anyhow::{Context, Result};
use reqwest::Client;

use crate::{config::Config, providers::types};

use super::http::{error_for_status_with_body, response_preview, ModelsResponse};

pub async fn list_models(api_key: &str) -> Result<Vec<String>> {
    let response = Client::new()
        .get("https://api.openai.com/v1/models")
        .bearer_auth(api_key)
        .send()
        .await
        .context("OpenAI models request failed")?;
    let response = error_for_status_with_body(response, "listing OpenAI models").await?;

    let models: ModelsResponse = response
        .json()
        .await
        .context("invalid OpenAI models JSON")?;
    let mut ids: Vec<String> = models
        .data
        .into_iter()
        .map(|model| model.id)
        .filter(|id| is_bender_model_candidate(id))
        .collect();
    ids.sort_by(|a, b| model_rank(a).cmp(&model_rank(b)).then_with(|| a.cmp(b)));
    ids.dedup();
    Ok(ids)
}

pub async fn respond(config: &Config, prompt: &str) -> Result<types::AgentResponse> {
    let body = serde_json::json!({
        "model": config.model,
        "input": prompt
    });

    let response = Client::new()
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(config.openai_api_key()?)
        .json(&body)
        .send()
        .await
        .context("OpenAI request failed")?;
    let response = error_for_status_with_body(response, "creating OpenAI response").await?;

    let value: serde_json::Value = response.json().await.context("invalid OpenAI JSON")?;
    ensure_completed_response(&value)?;
    let text = extract_output_text(&value).with_context(|| {
        format!(
            "OpenAI response did not contain output text. Response preview: {}",
            response_preview(&value)
        )
    })?;
    types::normalize_text(&text)
}

fn is_bender_model_candidate(id: &str) -> bool {
    (id.starts_with("gpt-") || id.starts_with("codex-"))
        && !id.contains("audio")
        && !id.contains("image")
        && !id.contains("realtime")
        && !id.contains("search")
        && !id.contains("transcribe")
        && !id.contains("tts")
        && !id.contains("moderation")
        && !id.contains("embedding")
}

fn model_rank(id: &str) -> u8 {
    if id.contains("codex") && !id.contains("mini") {
        0
    } else if id.contains("codex") {
        1
    } else if id.starts_with("gpt-5") && !id.contains("mini") && !id.contains("nano") {
        2
    } else if id.starts_with("gpt-5") {
        3
    } else {
        4
    }
}

fn extract_output_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(|value| value.as_str()) {
        return Some(text.to_string());
    }

    let mut out = String::new();
    collect_text(value.get("output")?, &mut out);
    (!out.trim().is_empty()).then_some(out)
}

fn collect_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_text(value, out);
            }
        }
        serde_json::Value::Object(map) => {
            let is_text_content = map
                .get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|kind| {
                    matches!(kind, "output_text" | "text" | "summary_text" | "message")
                });
            if is_text_content {
                if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
                    out.push_str(text);
                } else if let Some(text) = map.get("content").and_then(|value| value.as_str()) {
                    out.push_str(text);
                }
            }
            for key in ["content", "output", "items"] {
                if let Some(value) = map.get(key) {
                    collect_text(value, out);
                }
            }
        }
        _ => {}
    }
}

fn ensure_completed_response(value: &serde_json::Value) -> Result<()> {
    if let Some(error) = value.get("error").filter(|value| !value.is_null()) {
        anyhow::bail!("OpenAI response error: {error}");
    }

    if let Some(status) = value.get("status").and_then(|value| value.as_str()) {
        if status != "completed" {
            let details = value
                .get("incomplete_details")
                .filter(|value| !value.is_null())
                .map(|value| value.to_string())
                .unwrap_or_else(|| "no details".to_string());
            anyhow::bail!("OpenAI response status was {status}: {details}");
        }
    }

    Ok(())
}
