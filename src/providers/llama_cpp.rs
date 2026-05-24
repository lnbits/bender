use anyhow::Result;

use crate::{config::Config, providers::types};

use super::{
    http::{list_openai_compatible_models, trim_base_url},
    openai_compatible,
};

pub async fn list_models(config: &Config) -> Result<Vec<String>> {
    list_openai_compatible_models(
        &format!("{}/v1/models", trim_base_url(&config.llama_cpp_base_url)),
        config.llama_cpp_api_key.as_deref(),
        "listing llama.cpp models",
    )
    .await
}

pub async fn respond(config: &Config, prompt: &str) -> Result<types::AgentResponse> {
    openai_compatible::chat(
        &format!(
            "{}/v1/chat/completions",
            trim_base_url(&config.llama_cpp_base_url)
        ),
        config.api_key()?,
        &config.model,
        prompt,
        "creating llama.cpp response",
    )
    .await
}
