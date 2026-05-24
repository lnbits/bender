use anyhow::Result;

use crate::{config::Config, providers::types};

use super::openai_compatible;

pub async fn list_models(_config: &Config) -> Result<Vec<String>> {
    Ok(Vec::new())
}

pub async fn respond(config: &Config, prompt: &str) -> Result<types::AgentResponse> {
    openai_compatible::chat(
        "https://api.deepseek.com/chat/completions",
        config.api_key()?,
        &config.model,
        prompt,
        "creating DeepSeek response",
    )
    .await
}
