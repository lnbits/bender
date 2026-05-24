use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::header,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;

use crate::{
    config::{
        available_models, is_allowed_model_id, providers, Config, Provider, ToolPathConfig,
        BENDER_NAME,
    },
    nostr_agent, patch, providers, tools,
};

#[derive(Clone)]
pub struct AppState {
    pub project_root: PathBuf,
    pub config: Arc<Mutex<Config>>,
    pending_tool: Arc<Mutex<Option<tools::PendingToolCall>>>,
}

impl AppState {
    pub fn new(project_root: PathBuf, config: Config) -> Self {
        Self {
            project_root,
            config: Arc::new(Mutex::new(config)),
            pending_tool: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    version: String,
    project_root: String,
    project_warning: Option<String>,
    name: String,
    npub: String,
    controller_npub: Option<String>,
    provider: Provider,
    model: String,
    models: Vec<String>,
    model_catalog: Vec<ProviderModelsResponse>,
    providers: Vec<ProviderResponse>,
    has_provider_auth: bool,
    ollama_base_url: String,
    llama_cpp_base_url: String,
    relays: Vec<String>,
    tool_paths: Vec<ToolPathResponse>,
    tools: Vec<tools::Tool>,
}

#[derive(Debug, Serialize)]
struct ProviderResponse {
    id: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct ProviderModelsResponse {
    provider: String,
    models: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SaveConfigRequest {
    controller_npub: Option<String>,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    deepseek_api_key: Option<String>,
    provider: Provider,
    model: String,
    ollama_base_url: Option<String>,
    llama_cpp_base_url: Option<String>,
    llama_cpp_api_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct SaveConfigResponse {
    ok: bool,
    profile_published: bool,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    models: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ToolPathResponse {
    path: String,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct AddToolPathRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct PickToolFolderResponse {
    path: String,
}

#[derive(Debug, Deserialize)]
struct AskRequest {
    instruction: String,
}

#[derive(Debug, Serialize)]
struct AskResponse {
    message: String,
    changed: bool,
    pending_tool: Option<tools::PendingToolCall>,
    dm_status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApproveToolRequest {
    id: String,
}

#[derive(Debug, Serialize)]
struct ApproveToolResponse {
    message: String,
    output: serde_json::Value,
    dm_status: Option<String>,
}

pub async fn bind(state: &AppState) -> Result<tokio::net::TcpListener> {
    let bind = state.config.lock().await.bind;
    tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("could not bind local web UI to http://{bind}"))
}

pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/logo.png", get(logo))
        .route("/api/status", get(status))
        .route("/api/config", post(save_config))
        .route("/api/models", get(models))
        .route("/api/ask", post(ask))
        .route("/api/tools/pick-folder", post(pick_tool_folder))
        .route("/api/tools/path", post(add_tool_path))
        .route("/api/tools/approve", post(approve_tool))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

async fn logo() -> impl IntoResponse {
    static LOGO_PNG: &[u8] = include_bytes!("../logo.png");
    ([(header::CONTENT_TYPE, "image/png")], LOGO_PNG)
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let config = state.config.lock().await;
    let discovered_tools = tools::discover(&config, &state.project_root).unwrap_or_default();
    let project_root = state.project_root.canonicalize().ok();
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        project_root: state.project_root.display().to_string(),
        project_warning: project_warning(&state.project_root),
        name: BENDER_NAME.to_string(),
        npub: config.public_key.clone(),
        controller_npub: config.controller_npub.clone(),
        provider: config.provider,
        model: config.model.clone(),
        models: available_models(config.provider)
            .iter()
            .map(|model| model.to_string())
            .collect(),
        model_catalog: providers()
            .iter()
            .map(|provider| ProviderModelsResponse {
                provider: provider.as_str().to_string(),
                models: available_models(*provider)
                    .iter()
                    .map(|model| model.to_string())
                    .collect(),
            })
            .collect(),
        providers: providers()
            .iter()
            .map(|provider| ProviderResponse {
                id: provider.as_str().to_string(),
                label: provider.label().to_string(),
            })
            .collect(),
        has_provider_auth: config.has_provider_auth(),
        ollama_base_url: config.ollama_base_url.clone(),
        llama_cpp_base_url: config.llama_cpp_base_url.clone(),
        relays: config.relays.clone(),
        tool_paths: config
            .tool_paths
            .iter()
            .filter(|tool_path| {
                project_root
                    .as_ref()
                    .and_then(|root| tool_path.path.canonicalize().ok().map(|path| (root, path)))
                    .is_none_or(|(root, path)| !path.starts_with(root))
            })
            .map(|tool_path| ToolPathResponse {
                path: tool_path.path.display().to_string(),
                enabled: tool_path.enabled,
            })
            .collect(),
        tools: discovered_tools,
    })
}

async fn save_config(
    State(state): State<AppState>,
    Json(request): Json<SaveConfigRequest>,
) -> Result<Json<SaveConfigResponse>, AppError> {
    if !is_allowed_model_id(request.provider, &request.model) {
        return Err(anyhow::anyhow!("unsupported model").into());
    }
    let mut config = state.config.lock().await;
    config.name = BENDER_NAME.to_string();
    config.provider = request.provider;
    config.model = request.model;
    config.controller_npub = request
        .controller_npub
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(api_key) = request.openai_api_key {
        let api_key = api_key.trim().to_string();
        if !api_key.is_empty() {
            config.openai_api_key = Some(api_key);
        }
    }
    if let Some(api_key) = request.anthropic_api_key {
        let api_key = api_key.trim().to_string();
        if !api_key.is_empty() {
            config.anthropic_api_key = Some(api_key);
        }
    }
    if let Some(api_key) = request.deepseek_api_key {
        let api_key = api_key.trim().to_string();
        if !api_key.is_empty() {
            config.deepseek_api_key = Some(api_key);
        }
    }
    if let Some(base_url) = request.ollama_base_url {
        let base_url = base_url.trim().to_string();
        if !base_url.is_empty() {
            config.ollama_base_url = base_url;
        }
    }
    if let Some(base_url) = request.llama_cpp_base_url {
        let base_url = base_url.trim().to_string();
        if !base_url.is_empty() {
            config.llama_cpp_base_url = base_url;
        }
    }
    if let Some(api_key) = request.llama_cpp_api_key {
        let api_key = api_key.trim().to_string();
        if !api_key.is_empty() {
            config.llama_cpp_api_key = Some(api_key);
        }
    }
    config.controller()?;
    config.save(&state.project_root)?;
    let saved_config = config.clone();
    drop(config);
    let profile_published = match nostr_agent::publish_profile(&saved_config).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(error = %err, "could not publish nostr profile metadata");
            false
        }
    };
    Ok(Json(SaveConfigResponse {
        ok: true,
        profile_published,
    }))
}

async fn models(State(state): State<AppState>) -> Result<Json<ModelsResponse>, AppError> {
    let config = state.config.lock().await.clone();
    let mut models: Vec<String> = available_models(config.provider)
        .iter()
        .map(|model| model.to_string())
        .collect();
    models.extend(providers::list_models(&config).await?);
    models.sort();
    models.dedup();
    Ok(Json(ModelsResponse { models }))
}

async fn add_tool_path(
    State(state): State<AppState>,
    Json(request): Json<AddToolPathRequest>,
) -> Result<Json<SaveConfigResponse>, AppError> {
    let mut config = state.config.lock().await;
    config.tool_paths.clear();

    let path = request.path.trim();
    if path.is_empty() {
        config.save(&state.project_root)?;
        return Err(anyhow::anyhow!("tool path is required").into());
    }
    let path = match std::path::PathBuf::from(path).canonicalize() {
        Ok(path) => path,
        Err(err) => {
            config.save(&state.project_root)?;
            return Err(anyhow::anyhow!("could not read tool path: {path}: {err}").into());
        }
    };
    let project_root = state
        .project_root
        .canonicalize()
        .context("could not canonicalize project root")?;
    if path.starts_with(&project_root) {
        config.save(&state.project_root)?;
        return Err(anyhow::anyhow!(
            "Bender can not use tools in the folder where he lives, because he could bend them"
        )
        .into());
    }
    if !path.is_dir() {
        config.save(&state.project_root)?;
        return Err(anyhow::anyhow!("tool path must be a folder").into());
    }
    if !path.join("bender-tool.toml").exists()
        && !std::fs::read_dir(&path)?
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.path().join("bender-tool.toml").exists())
    {
        config.save(&state.project_root)?;
        return Err(anyhow::anyhow!(
            "tool path must contain bender-tool.toml or tool folders"
        )
        .into());
    }

    config.tool_paths.push(ToolPathConfig {
        path,
        enabled: true,
    });
    config.save(&state.project_root)?;
    Ok(Json(SaveConfigResponse {
        ok: true,
        profile_published: false,
    }))
}

async fn pick_tool_folder() -> Result<Json<PickToolFolderResponse>, AppError> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("command -v zenity >/dev/null 2>&1 && zenity --file-selection --directory")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to open folder picker")?;
    if !output.status.success() {
        return Err(anyhow::anyhow!("folder picker is not available; paste the folder path instead").into());
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(anyhow::anyhow!("no folder selected").into());
    }
    Ok(Json(PickToolFolderResponse { path }))
}

async fn ask(
    State(state): State<AppState>,
    Json(request): Json<AskRequest>,
) -> Result<Json<AskResponse>, AppError> {
    let config = state.config.lock().await.clone();
    let available_tools = tools::discover(&config, &state.project_root)?;
    let tools_prompt = tools::prompt_section(&available_tools);
    let response =
        providers::respond(&config, &state.project_root, &request.instruction, &tools_prompt)
            .await?;

    let changed = !response.diff.trim().is_empty();
    if changed {
        patch::validate_patch(&state.project_root, &response.diff)?;
        patch::store_last_patch(&state.project_root, &response.diff)?;
        patch::apply_last_patch(&state.project_root).await?;
    }

    let mut message = if changed {
        format!("{}\n\nDone.", response.summary)
    } else {
        response.summary.clone()
    };
    let pending_tool = if let Some(call) = response.tool_calls.first() {
        let tool = tools::find_tool(&available_tools, &call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", call.name))?;
        if tool.requires_confirmation {
            let pending = tools::PendingToolCall {
                id: new_pending_tool_id(),
                name: tool.name,
                description: tool.description,
                permissions: tool.permissions,
                input: call.input.clone(),
            };
            *state.pending_tool.lock().await = Some(pending.clone());
            message.push_str("\n\nTool approval needed in the web UI.");
            Some(pending)
        } else {
            let result = tools::execute(&tool, &state.project_root, &call.input).await?;
            let message = format!(
                "{}\n\nTool {} completed:\n{}",
                message, result.name, result.output
            );
            let dm_status = dm_status(&config, &message).await;
            return Ok(Json(AskResponse {
                message,
                changed,
                pending_tool: None,
                dm_status,
            }));
        }
    } else {
        None
    };
    let dm_status = dm_status(&config, &message).await;

    Ok(Json(AskResponse {
        message,
        changed,
        pending_tool,
        dm_status,
    }))
}

async fn approve_tool(
    State(state): State<AppState>,
    Json(request): Json<ApproveToolRequest>,
) -> Result<Json<ApproveToolResponse>, AppError> {
    let pending = {
        let mut pending_tool = state.pending_tool.lock().await;
        let pending = pending_tool
            .take()
            .ok_or_else(|| anyhow::anyhow!("no pending tool approval"))?;
        if pending.id != request.id {
            *pending_tool = Some(pending);
            return Err(anyhow::anyhow!("pending tool approval did not match").into());
        }
        pending
    };
    let config = state.config.lock().await.clone();
    let available_tools = tools::discover(&config, &state.project_root)?;
    let tool = tools::find_tool(&available_tools, &pending.name)
        .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", pending.name))?;
    let result = tools::execute(&tool, &state.project_root, &pending.input).await?;
    let message = format!("Tool {} completed.", result.name);
    let dm_status = dm_status(
        &config,
        &format!("{}\n{}", message, serde_json::to_string_pretty(&result.output)?),
    )
    .await;
    Ok(Json(ApproveToolResponse {
        message,
        output: result.output,
        dm_status,
    }))
}

async fn dm_status(config: &Config, message: &str) -> Option<String> {
    match nostr_agent::send_controller_dm(config, message).await {
        Ok(true) => Some("DM sent to controller.".to_string()),
        Ok(false) => None,
        Err(err) => Some(format!("DM failed: {err}")),
    }
}

fn new_pending_tool_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("tool-{millis}")
}

struct AppError(anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (axum::http::StatusCode::BAD_REQUEST, self.0.to_string()).into_response()
    }
}

const INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Bender</title>
  <style>
    :root { color-scheme: dark; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    body { margin: 0; background: #101827; color: #f4f7fb; }
    main { max-width: 1040px; margin: 0 auto; padding: 28px; }
    header { display: flex; align-items: end; justify-content: space-between; gap: 16px; border-bottom: 1px solid #2b3447; padding-bottom: 18px; }
    footer { margin-top: 26px; border-top: 1px solid #2b3447; padding-top: 16px; text-align: center; }
    .brand { display: flex; align-items: center; gap: 12px; min-width: 0; }
    .brand-logo { width: min(300px, 70vw); max-height: 96px; object-fit: contain; flex: 0 0 auto; }
    .muted { color: #9aa9bd; font-size: 14px; }
    .header-meta { display: flex; flex-direction: column; align-items: end; gap: 10px; min-width: 0; }
    .tool-badges { display: flex; flex-wrap: wrap; justify-content: end; gap: 6px; max-width: 460px; }
    .tool-badge { border: 1px solid #64b6e4; border-radius: 999px; padding: 4px 9px; background: #142238; color: #dff3ff; font-size: 12px; font-weight: 650; }
    .grid { display: grid; grid-template-columns: minmax(0, 1fr) 320px; gap: 20px; margin-top: 22px; }
    section, aside { min-width: 0; }
    textarea, pre, input, select { width: 100%; box-sizing: border-box; border: 1px solid #2d405d; border-radius: 8px; background: #111d30; color: #f4f7fb; }
    textarea, pre { background: #f4f0e6; color: #182234; border-color: #9eb2c6; }
    textarea { min-height: 160px; padding: 14px; font: inherit; resize: vertical; }
    input, select { height: 38px; padding: 0 10px; margin: 6px 0 12px; font: inherit; }
    textarea::placeholder, input::placeholder { color: #73859d; }
    textarea::placeholder { color: #6f7d8d; }
    textarea:focus, input:focus, select:focus { outline: 2px solid #64b6e4; border-color: #64b6e4; }
    label { display: block; margin-top: 10px; color: #d5deea; font-size: 13px; font-weight: 650; }
    pre { min-height: 360px; overflow: auto; padding: 14px; font-size: 13px; line-height: 1.45; white-space: pre-wrap; }
    button { border: 0; border-radius: 8px; padding: 10px 14px; background: #64b6e4; color: #07111e; font-weight: 650; cursor: pointer; }
    button.secondary { background: #273349; color: #eef6ff; }
    button:disabled { opacity: .55; cursor: wait; }
    .actions { display: flex; gap: 10px; margin: 12px 0; }
    .tool-path-actions { display: grid; grid-template-columns: 1fr auto; gap: 8px; align-items: center; }
    .tool-approval { display: none; margin: 12px 0; padding: 12px; border: 1px solid #64b6e4; border-radius: 8px; background: #111d30; color: #f4f7fb; }
    .tool-approval strong { display: block; margin-bottom: 6px; }
    .tool-approval code { display: block; margin: 8px 0; white-space: pre-wrap; overflow-wrap: anywhere; color: #d5deea; font-size: 12px; }
    .tool-list { margin: 8px 0 12px; padding: 0; list-style: none; }
    .tool-list li { margin: 6px 0; padding: 8px; border: 1px solid #2d405d; border-radius: 8px; background: #111d30; font-size: 13px; }
    .tool-list strong { display: block; color: #f4f7fb; }
    .provider-field { display: none; }
    .provider-field.is-visible { display: block; }
    .warning { display: none; margin-bottom: 14px; padding: 10px 12px; border: 1px solid #b57d20; border-radius: 8px; background: #2d2106; color: #ffd887; font-size: 14px; }
    dl { margin: 0; }
    dt { margin-top: 12px; font-size: 12px; color: #9aa9bd; text-transform: uppercase; }
    dd { margin: 4px 0 0; overflow-wrap: anywhere; font-size: 14px; }
    .copy-value { cursor: pointer; border-radius: 6px; padding: 4px 6px; margin-left: -6px; }
    .copy-value:hover { background: #1c2b42; color: #ffffff; }
    @media (max-width: 780px) {
      main { padding: 18px; }
      header { align-items: start; flex-direction: column; }
      .header-meta { align-items: start; width: 100%; }
      .tool-badges { justify-content: start; max-width: 100%; }
      .grid { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <main>
    <header>
      <div class="brand">
        <img class="brand-logo" src="/logo.png" alt="Bender">
      </div>
      <div class="header-meta">
        <div id="headerTools" class="tool-badges"></div>
        <div class="muted" id="project"></div>
      </div>
    </header>
    <div class="grid">
      <section>
        <div id="warning" class="warning"></div>
        <textarea id="instruction" placeholder="Ask Bender anything..."></textarea>
        <div class="actions">
          <button id="send">Send</button>
        </div>
        <div id="toolApproval" class="tool-approval">
          <strong id="toolTitle"></strong>
          <div id="toolPermissions"></div>
          <code id="toolInput"></code>
          <button id="approveTool">Approve Tool</button>
        </div>
        <pre id="output"></pre>
      </section>
      <aside>
        <label for="provider">Provider</label>
        <select id="provider"></select>
        <label for="model">Model</label>
        <select id="model"></select>
        <button class="secondary" id="refreshModels">Refresh Models</button>
        <label for="controllerInput">Controller npub</label>
        <input id="controllerInput" spellcheck="false" placeholder="npub1..." />
        <div class="provider-field" data-provider-field="openai">
          <label for="apiKey">OpenAI API key</label>
          <input id="apiKey" type="password" spellcheck="false" placeholder="sk-..." />
        </div>
        <div class="provider-field" data-provider-field="anthropic">
          <label for="anthropicApiKey">Claude API key</label>
          <input id="anthropicApiKey" type="password" spellcheck="false" placeholder="sk-ant-..." />
        </div>
        <div class="provider-field" data-provider-field="deepseek">
          <label for="deepseekApiKey">DeepSeek API key</label>
          <input id="deepseekApiKey" type="password" spellcheck="false" placeholder="sk-..." />
        </div>
        <div class="provider-field" data-provider-field="ollama">
          <label for="ollamaBaseUrl">Ollama URL</label>
          <input id="ollamaBaseUrl" spellcheck="false" placeholder="http://127.0.0.1:11434" />
        </div>
        <div class="provider-field" data-provider-field="llama_cpp">
          <label for="llamaCppBaseUrl">llama.cpp URL</label>
          <input id="llamaCppBaseUrl" spellcheck="false" placeholder="http://127.0.0.1:8080" />
          <label for="llamaCppApiKey">llama.cpp API key</label>
          <input id="llamaCppApiKey" type="password" spellcheck="false" placeholder="optional" />
        </div>
        <button id="saveConfig">Save Setup</button>
        <label for="toolPathInput">Tool folder</label>
        <div class="tool-path-actions">
          <input id="toolPathInput" spellcheck="false" placeholder="/path/to/tool-or-tools-folder" />
          <button class="secondary" id="chooseToolPath">Choose</button>
        </div>
        <button class="secondary" id="addToolPath">Add Tool Folder</button>
        <dl>
          <dt>tool paths</dt><dd id="toolPaths"></dd>
        </dl>
        <dl>
          <dt>npub</dt><dd id="npub" class="copy-value" title="Copy npub"></dd>
          <dt>version</dt><dd id="version"></dd>
          <dt>auth</dt><dd id="auth"></dd>
          <dt>relays</dt><dd id="relays"></dd>
        </dl>
      </aside>
    </div>
    <footer class="muted">Made by the LNbits team</footer>
  </main>
  <script>
    const $ = id => document.getElementById(id);
    let pendingTool = null;
    async function api(path, body) {
      const res = await fetch(path, {
        method: body ? 'POST' : 'GET',
        headers: body ? { 'content-type': 'application/json' } : {},
        body: body ? JSON.stringify(body) : undefined
      });
      const text = await res.text();
      if (!res.ok) throw new Error(text);
      return text ? JSON.parse(text) : {};
    }
    async function load() {
      const status = await api('/api/status');
      document.title = `Bender ${status.version}`;
      $('project').textContent = status.project_root;
      if (status.project_warning) {
        $('warning').textContent = status.project_warning;
        $('warning').style.display = 'block';
      } else {
        $('warning').style.display = 'none';
      }
      $('npub').textContent = status.npub;
      $('provider').innerHTML = status.providers.map(provider => `<option value="${provider.id}">${provider.label}</option>`).join('');
      $('provider').value = status.provider;
      window.modelCatalog = Object.fromEntries(status.model_catalog.map(item => [item.provider, item.models]));
      setModels(status.models);
      $('model').value = status.model;
      $('controllerInput').value = status.controller_npub || '';
      $('ollamaBaseUrl').value = status.ollama_base_url;
      $('llamaCppBaseUrl').value = status.llama_cpp_base_url;
      syncProviderFields();
      $('version').textContent = status.version;
      $('auth').textContent = status.has_provider_auth ? 'provider auth ready' : 'provider auth missing';
      $('relays').textContent = status.relays.join(', ');
      renderTools(status.tools, status.tool_paths);
    }
    function setModels(models) {
      $('model').innerHTML = models.map(model => `<option value="${model}">${model}</option>`).join('');
    }
    function syncProviderFields() {
      const provider = $('provider').value;
      document.querySelectorAll('[data-provider-field]').forEach(field => {
        field.classList.toggle('is-visible', field.dataset.providerField === provider);
      });
    }
    async function copyText(value, copiedMessage) {
      if (!value) return;
      await navigator.clipboard.writeText(value);
      $('output').textContent = copiedMessage;
    }
    function showPendingTool(tool) {
      pendingTool = tool;
      if (!tool) {
        $('toolApproval').style.display = 'none';
        return;
      }
      $('toolTitle').textContent = `Approve ${tool.name}`;
      $('toolPermissions').textContent = tool.permissions.length ? `Permissions: ${tool.permissions.join(', ')}` : 'Permissions: none';
      $('toolInput').textContent = JSON.stringify(tool.input, null, 2);
      $('toolApproval').style.display = 'block';
    }
    function renderTools(tools, toolPaths) {
      $('headerTools').innerHTML = tools.length
        ? tools.map(tool => `<span class="tool-badge" title="${tool.description}">${tool.name}</span>`).join('')
        : '';
      $('toolPaths').textContent = toolPaths.length
        ? toolPaths.map(toolPath => toolPath.path).join(', ')
        : 'none';
    }
    $('saveConfig').onclick = async () => {
      $('saveConfig').disabled = true;
      try {
        const result = await api('/api/config', {
          provider: $('provider').value,
          model: $('model').value,
          controller_npub: $('controllerInput').value,
          openai_api_key: $('apiKey').value,
          anthropic_api_key: $('anthropicApiKey').value,
          deepseek_api_key: $('deepseekApiKey').value,
          ollama_base_url: $('ollamaBaseUrl').value,
          llama_cpp_base_url: $('llamaCppBaseUrl').value,
          llama_cpp_api_key: $('llamaCppApiKey').value
        });
        $('apiKey').value = '';
        $('anthropicApiKey').value = '';
        $('deepseekApiKey').value = '';
        $('llamaCppApiKey').value = '';
        await load();
        $('output').textContent = result.profile_published
          ? 'I published my Nostr profile! Bite my shiny metal ass.'
          : 'Setup saved, but I could not publish my Nostr profile.';
      } catch (err) {
        await load();
        $('output').textContent = err.message;
      } finally {
        $('saveConfig').disabled = false;
      }
    };
    $('refreshModels').onclick = async () => {
      $('refreshModels').disabled = true;
      try {
        const current = $('model').value;
        const result = await api('/api/models');
        $('model').innerHTML = result.models.map(model => `<option value="${model}">${model}</option>`).join('');
        if (result.models.includes(current)) $('model').value = current;
        $('output').textContent = 'Model list refreshed.';
      } catch (err) {
        $('output').textContent = err.message;
      } finally {
        $('refreshModels').disabled = false;
      }
    };
    $('chooseToolPath').onclick = async () => {
      try {
        const result = await api('/api/tools/pick-folder', {});
        $('toolPathInput').value = result.path;
      } catch (err) {
        $('output').textContent = err.message;
      }
    };
    $('addToolPath').onclick = async () => {
      $('addToolPath').disabled = true;
      try {
        await api('/api/tools/path', { path: $('toolPathInput').value });
        $('toolPathInput').value = '';
        await load();
        $('output').textContent = 'Tool folder added.';
      } catch (err) {
        $('output').textContent = err.message;
      } finally {
        $('addToolPath').disabled = false;
      }
    };
    $('send').onclick = async () => {
      $('send').disabled = true;
      $('output').textContent = 'Thinking...';
      try {
        const result = await api('/api/ask', { instruction: $('instruction').value });
        showPendingTool(result.pending_tool);
        $('output').textContent = result.dm_status
          ? `${result.message}\n\n${result.dm_status}`
          : result.message;
      } catch (err) {
        $('output').textContent = err.message;
      } finally {
        $('send').disabled = false;
      }
    };
    $('approveTool').onclick = async () => {
      if (!pendingTool) return;
      $('approveTool').disabled = true;
      try {
        const result = await api('/api/tools/approve', { id: pendingTool.id });
        showPendingTool(null);
        const toolOutput = `${result.message}\n${JSON.stringify(result.output, null, 2)}`;
        $('output').textContent = result.dm_status
          ? `${toolOutput}\n\n${result.dm_status}`
          : toolOutput;
      } catch (err) {
        $('output').textContent = err.message;
      } finally {
        $('approveTool').disabled = false;
      }
    };
    $('provider').onchange = () => {
      const models = window.modelCatalog?.[$('provider').value] || ['local-model'];
      setModels(models);
      $('model').value = models[0];
      syncProviderFields();
    };
    $('npub').onclick = () => copyText($('npub').textContent, 'npub copied.');
    load().catch(err => $('output').textContent = err.message);
  </script>
</body>
</html>"#;

fn project_warning(project_root: &std::path::Path) -> Option<String> {
    let components: Vec<_> = project_root
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    let is_cargo_output = components
        .windows(2)
        .any(|window| window == ["target", "release"] || window == ["target", "debug"]);

    is_cargo_output.then(|| {
        "You are running Bender from a Cargo build output folder. Copy the binary into the project folder you want controlled and run it there.".to_string()
    })
}
