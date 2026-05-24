use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub summary: String,
    #[serde(default)]
    pub diff: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

pub fn normalize_text(text: &str) -> Result<AgentResponse> {
    match serde_json::from_str::<AgentResponse>(text) {
        Ok(response) => Ok(response),
        Err(_) => Ok(AgentResponse {
            summary: text.trim().to_string(),
            diff: String::new(),
            tool_calls: Vec::new(),
        }),
    }
}
