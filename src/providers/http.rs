use anyhow::{Context, Result};
use reqwest::{Client, Response};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ModelInfo {
    pub id: String,
}

pub async fn error_for_status_with_body(response: Response, action: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|err| format!("could not read error body: {err}"));
    anyhow::bail!("provider error while {action}: HTTP {status}: {body}");
}

pub async fn list_openai_compatible_models(
    url: &str,
    api_key: Option<&str>,
    action: &str,
) -> Result<Vec<String>> {
    let mut request = Client::new().get(url);
    if let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("{action} failed"))?;
    let response = error_for_status_with_body(response, action).await?;
    let models: ModelsResponse = response.json().await.context("invalid models JSON")?;
    let mut ids: Vec<String> = models.data.into_iter().map(|model| model.id).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub fn response_preview(value: &serde_json::Value) -> String {
    let mut preview = value.to_string();
    const MAX_PREVIEW: usize = 1600;
    if preview.len() > MAX_PREVIEW {
        preview.truncate(MAX_PREVIEW);
        preview.push_str("...");
    }
    preview
}

pub fn trim_base_url(base_url: &str) -> &str {
    base_url.trim().trim_end_matches('/')
}
