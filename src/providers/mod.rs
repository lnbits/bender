mod anthropic;
mod deepseek;
mod http;
mod llama_cpp;
mod ollama;
mod openai;
mod openai_compatible;
mod types;

use anyhow::Result;

use crate::{
    config::{Config, Provider},
    project,
};

pub use types::{AgentResponse, PromptImage};

pub async fn list_models(config: &Config) -> Result<Vec<String>> {
    match config.provider {
        Provider::Openai => openai::list_models(config.openai_api_key()?).await,
        Provider::Anthropic => anthropic::list_models(config).await,
        Provider::Deepseek => deepseek::list_models(config).await,
        Provider::Ollama => ollama::list_models(config).await,
        Provider::LlamaCpp => llama_cpp::list_models(config).await,
    }
}

pub async fn respond(
    config: &Config,
    project_root: &std::path::Path,
    instruction: &str,
    tools_prompt: &str,
    conversation_prompt: &str,
    images: &[PromptImage],
) -> Result<AgentResponse> {
    let context = project::collect_context(project_root)?;
    let prompt = build_prompt(
        instruction,
        &context,
        tools_prompt,
        conversation_prompt,
        images,
    );

    match config.provider {
        Provider::Openai => openai::respond(config, &prompt, images).await,
        Provider::Anthropic => anthropic::respond(config, &prompt, images).await,
        Provider::Deepseek => deepseek::respond(config, &prompt).await,
        Provider::Ollama => ollama::respond(config, &prompt).await,
        Provider::LlamaCpp => llama_cpp::respond(config, &prompt).await,
    }
}

fn build_prompt(
    instruction: &str,
    context: &str,
    tools_prompt: &str,
    conversation_prompt: &str,
    images: &[PromptImage],
) -> String {
    let image_note = if images.is_empty() {
        "Attached images: none.".to_string()
    } else {
        format!(
            "Attached images:\n{}",
            images
                .iter()
                .map(|image| format!("- {} ({})", image.name, image.media_type))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    format!(
        r#"You are Bender, a local coding agent.

Rules:
- Answer normal questions directly.
- If a file change or tool action is needed, return a JSON object with keys "summary", "diff", and "tool_calls".
- In that JSON, "diff" must be a valid unified diff suitable for `git apply`.
- If you are not changing files, "diff" must be an empty string.
- In that JSON, "tool_calls" must be an array of objects with keys "name" and "input".
- Patch only files in the supplied project context.
- Do not include shell commands.
- Do not modify .bender, .git, target, node_modules, or secrets.
- Use tools only when the user explicitly asks for the capability or the tool is necessary to complete the request.
- Use only the tools listed below. Do not invent shell commands or unavailable tools.
- If no file change or tool call is needed, answer in plain text or return JSON with an empty diff and empty tool_calls.
- Do not talk about patches unless the user asks about implementation details.

{tools_prompt}

Conversation so far:
{conversation_prompt}

{image_note}

User request:
{instruction}

{context}
"#
    )
}
