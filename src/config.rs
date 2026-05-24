use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub const BENDER_NAME: &str = "Bender";
pub const BENDER_BIO: &str = "I bend things";
pub const BENDER_PROFILE_PICTURE_URL: &str =
    "https://raw.githubusercontent.com/lnbits/bender/main/profile.png";
pub const BENDER_PROFILE_BANNER_URL: &str =
    "https://raw.githubusercontent.com/lnbits/bender/main/bender.gif";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
    Deepseek,
    Ollama,
    LlamaCpp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_name")]
    pub name: String,
    pub secret_key: String,
    pub public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_npub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepseek_api_key: Option<String>,
    #[serde(default = "default_provider")]
    pub provider: Provider,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_ollama_base_url")]
    pub ollama_base_url: String,
    #[serde(default = "default_llama_cpp_base_url")]
    pub llama_cpp_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llama_cpp_api_key: Option<String>,
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_paths: Vec<ToolPathConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPathConfig {
    pub path: PathBuf,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_name() -> String {
    BENDER_NAME.to_string()
}

fn default_provider() -> Provider {
    Provider::Openai
}

fn default_model() -> String {
    "gpt-5.1-codex-mini".to_string()
}

fn default_ollama_base_url() -> String {
    "http://127.0.0.1:11434".to_string()
}

fn default_llama_cpp_base_url() -> String {
    "http://127.0.0.1:8080".to_string()
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:7331"
        .parse()
        .expect("valid default bind address")
}

fn default_relays() -> Vec<String> {
    vec![
        "wss://relay.damus.io".to_string(),
        "wss://nos.lol".to_string(),
        "wss://relay.primal.net".to_string(),
        "wss://relay.nostr.band".to_string(),
        "wss://relay.snort.social".to_string(),
        "wss://nostr.mom".to_string(),
        "wss://offchain.pub".to_string(),
        "wss://relay.current.fyi".to_string(),
        "wss://nostr.wine".to_string(),
        "wss://relayable.org".to_string(),
    ]
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn new(keys: Keys) -> Result<Self> {
        Ok(Self {
            name: default_name(),
            secret_key: keys.secret_key().to_bech32()?,
            public_key: keys.public_key().to_bech32()?,
            controller_npub: None,
            openai_api_key: None,
            anthropic_api_key: None,
            deepseek_api_key: None,
            provider: default_provider(),
            model: default_model(),
            ollama_base_url: default_ollama_base_url(),
            llama_cpp_base_url: default_llama_cpp_base_url(),
            llama_cpp_api_key: None,
            bind: default_bind(),
            relays: default_relays(),
            tool_paths: Vec::new(),
        })
    }

    pub fn path(project_root: &Path) -> PathBuf {
        project_root.join(".bender").join("config.toml")
    }

    pub fn load(project_root: &Path) -> Result<Self> {
        let path = Self::path(project_root);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&raw).with_context(|| format!("could not parse {}", path.display()))?;
        config.name = BENDER_NAME.to_string();
        Ok(config)
    }

    pub fn save(&self, project_root: &Path) -> Result<()> {
        let dir = project_root.join(".bender");
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let mut config = self.clone();
        config.name = BENDER_NAME.to_string();
        let raw = toml::to_string_pretty(&config).context("could not serialize config")?;
        fs::write(Self::path(project_root), raw).context("could not write config")
    }

    pub fn keys(&self) -> Result<Keys> {
        Keys::parse(&self.secret_key).context("invalid Bender secret_key")
    }

    pub fn controller(&self) -> Result<Option<PublicKey>> {
        self.controller_npub
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(PublicKey::from_bech32)
            .transpose()
            .context("invalid controller_npub")
    }

    pub fn api_key(&self) -> Result<Option<&str>> {
        let key = match self.provider {
            Provider::Openai => self.openai_api_key.as_deref(),
            Provider::Anthropic => self.anthropic_api_key.as_deref(),
            Provider::Deepseek => self.deepseek_api_key.as_deref(),
            Provider::Ollama => None,
            Provider::LlamaCpp => self.llama_cpp_api_key.as_deref(),
        };
        if matches!(self.provider, Provider::Ollama) {
            return Ok(None);
        }
        key.filter(|value| !value.trim().is_empty())
            .map(Some)
            .with_context(|| format!("missing API key for {}", self.provider.label()))
    }

    pub fn openai_api_key(&self) -> Result<&str> {
        self.openai_api_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .context("missing openai_api_key in .bender/config.toml or OPENAI_API_KEY")
    }

    pub fn has_provider_auth(&self) -> bool {
        match self.provider {
            Provider::Openai => has_value(&self.openai_api_key),
            Provider::Anthropic => has_value(&self.anthropic_api_key),
            Provider::Deepseek => has_value(&self.deepseek_api_key),
            Provider::Ollama => true,
            Provider::LlamaCpp => true,
        }
    }
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Openai => "OpenAI / Codex",
            Provider::Anthropic => "Claude",
            Provider::Deepseek => "DeepSeek",
            Provider::Ollama => "Ollama",
            Provider::LlamaCpp => "llama.cpp",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Openai => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Deepseek => "deepseek",
            Provider::Ollama => "ollama",
            Provider::LlamaCpp => "llama_cpp",
        }
    }
}

pub fn providers() -> &'static [Provider] {
    &[
        Provider::Openai,
        Provider::Anthropic,
        Provider::Deepseek,
        Provider::Ollama,
        Provider::LlamaCpp,
    ]
}

pub fn available_models(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Openai => &[
            "gpt-5.2-codex",
            "gpt-5.1-codex",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex-mini",
            "gpt-5-codex",
            "gpt-5.2",
            "gpt-5.1",
            "gpt-5-mini",
        ],
        Provider::Anthropic => &[
            "claude-sonnet-4-5",
            "claude-opus-4-1",
            "claude-haiku-4-5",
            "claude-3-5-sonnet-latest",
        ],
        Provider::Deepseek => &["deepseek-chat", "deepseek-reasoner"],
        Provider::Ollama => &[
            "qwen2.5-coder:7b",
            "qwen2.5-coder:14b",
            "deepseek-coder-v2:16b",
            "llama3.1:8b",
        ],
        Provider::LlamaCpp => &["local-model"],
    }
}

pub fn is_allowed_model_id(provider: Provider, model: &str) -> bool {
    let model = model.trim();
    if model.is_empty() || model.len() > 128 {
        return false;
    }
    if available_models(provider).contains(&model) {
        return true;
    }
    match provider {
        Provider::Openai => model.starts_with("gpt-") || model.starts_with("codex-"),
        Provider::Anthropic => model.starts_with("claude-"),
        Provider::Deepseek => model.starts_with("deepseek-"),
        Provider::Ollama | Provider::LlamaCpp => model
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':' | '/')),
    }
}

fn has_value(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}
