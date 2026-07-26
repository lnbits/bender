use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;

use crate::{
    chats,
    config::{
        available_models, is_allowed_model_id, providers, Config, Provider, ToolPathConfig,
        BENDER_NAME,
    },
    jobs::{append_jsonl, AcceptanceCriterion, JobRecord, JobState, JobStore},
    nostr_agent,
    orchestrator::{GemmaReviewer, Orchestrator, SharedReviewer},
    project_config::ProjectConfig,
    providers, tools,
    worker::CodexCliWorker,
};

#[derive(Clone)]
pub struct AppState {
    pub project_root: PathBuf,
    pub config: Arc<Mutex<Config>>,
    pub worker: Arc<CodexCliWorker>,
    pending_tool: Arc<Mutex<Option<tools::PendingToolCall>>>,
}

impl AppState {
    pub fn new(project_root: PathBuf, config: Config) -> Self {
        Self {
            project_root,
            config: Arc::new(Mutex::new(config)),
            worker: Arc::new(CodexCliWorker::default()),
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
    chats: Vec<chats::ChatSummary>,
    active_chat_id: String,
    messages: Vec<chats::ChatMessage>,
    jobs: Vec<JobRecord>,
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
struct AuthStatusResponse {
    configured: bool,
    authenticated: bool,
}

#[derive(Debug, Deserialize)]
struct AuthRequest {
    password: String,
}

#[derive(Debug, Serialize)]
struct AuthResponse {
    ok: bool,
    configured: bool,
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
    chat_id: Option<String>,
    #[serde(default)]
    images: Vec<AskImage>,
}

#[derive(Debug, Deserialize)]
struct AskImage {
    name: String,
    media_type: String,
    data_url: String,
}

#[derive(Debug, Serialize)]
struct AskResponse {
    message: String,
    changed: bool,
    pending_tool: Option<tools::PendingToolCall>,
    chat_id: String,
    chats: Vec<chats::ChatSummary>,
    messages: Vec<chats::ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct ApproveToolRequest {
    id: String,
}

#[derive(Debug, Serialize)]
struct ApproveToolResponse {
    message: String,
    output: serde_json::Value,
    chats: Vec<chats::ChatSummary>,
    messages: Vec<chats::ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct SelectChatRequest {
    id: String,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    chat_id: String,
    chats: Vec<chats::ChatSummary>,
    messages: Vec<chats::ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct JobActionRequest {
    id: String,
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
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/status", get(status))
        .route("/api/config", post(save_config))
        .route("/api/config/clear", post(clear_config))
        .route("/api/config/retire", post(retire_bender))
        .route("/api/models", get(models))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/cancel", post(cancel_job))
        .route("/api/jobs/retry", post(retry_job))
        .route("/api/chats/new", post(new_chat))
        .route("/api/chats/select", post(select_chat))
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

async fn auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<AuthStatusResponse> {
    let config = state.config.lock().await;
    Json(AuthStatusResponse {
        configured: config.auth_password_hash.is_some(),
        authenticated: is_authenticated(&config, &headers),
    })
}

async fn auth_login(
    State(state): State<AppState>,
    Json(request): Json<AuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let password = request.password.trim();
    if password.len() < 8 {
        return Err(anyhow::anyhow!("password must be at least 8 characters").into());
    }

    let mut config = state.config.lock().await;
    if config.auth_password_hash.is_none() {
        let salt = random_hex();
        config.auth_salt = Some(salt.clone());
        config.auth_password_hash = Some(password_hash(&salt, password));
    } else {
        let salt = config
            .auth_salt
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("auth salt missing"))?;
        let expected = config
            .auth_password_hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("auth password hash missing"))?;
        if password_hash(salt, password) != expected {
            return Err(anyhow::anyhow!("invalid password").into());
        }
    }

    let token = random_hex();
    config.auth_session_hash = Some(session_hash(&token));
    config.save(&state.project_root)?;
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, auth_cookie(&token).parse()?);
    Ok((
        headers,
        Json(AuthResponse {
            ok: true,
            configured: true,
        }),
    ))
}

async fn auth_logout(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let mut config = state.config.lock().await;
    config.auth_session_hash = None;
    config.save(&state.project_root)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        "bender_auth=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly".parse()?,
    );
    Ok((
        headers,
        Json(AuthResponse {
            ok: true,
            configured: true,
        }),
    ))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let config = state.config.lock().await;
    let discovered_tools = tools::discover(&config, &state.project_root).unwrap_or_default();
    let mut chat_store = chats::load(&state.project_root)?;
    let active_chat_id = chats::ensure_web_chat(&mut chat_store);
    chats::save(&state.project_root, &chat_store)?;
    Ok(Json(StatusResponse {
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
            .map(|tool_path| ToolPathResponse {
                path: tool_path.path.display().to_string(),
                enabled: tool_path.enabled,
            })
            .collect(),
        tools: discovered_tools,
        chats: chats::summaries(&chat_store),
        messages: chats::messages(&chat_store, &active_chat_id),
        active_chat_id,
        jobs: JobStore::new(&state.project_root)?.list()?,
    }))
}

async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<JobRecord>>, AppError> {
    require_auth(&state, &headers).await?;
    Ok(Json(JobStore::new(&state.project_root)?.list()?))
}

async fn cancel_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JobActionRequest>,
) -> Result<Json<JobRecord>, AppError> {
    require_auth(&state, &headers).await?;
    let store = JobStore::new(&state.project_root)?;
    let mut job = store.load(&request.id)?;
    if job.record.state.is_terminal() {
        return Err(anyhow::anyhow!("job is already terminal").into());
    }
    let invocation_id = format!("{}-attempt-{}", job.record.id, job.record.attempt);
    let _ = crate::worker::CodingWorker::cancel(state.worker.as_ref(), &invocation_id).await?;
    job.transition(JobState::Cancelled, "Cancelled by local controller")?;
    Ok(Json(job.record))
}

async fn retry_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JobActionRequest>,
) -> Result<Json<JobRecord>, AppError> {
    require_auth(&state, &headers).await?;
    let store = JobStore::new(&state.project_root)?;
    let mut job = store.load(&request.id)?;
    job.retry()?;
    let project = ProjectConfig::load(&state.project_root)?;
    let reviewer: Option<SharedReviewer> = project
        .reviewers
        .get("gemma")
        .filter(|settings| settings.enabled)
        .map(|settings| {
            Arc::new(GemmaReviewer::new(&settings.base_url, &settings.model)) as SharedReviewer
        });
    let orchestrator =
        Orchestrator::new(&state.project_root, project, state.worker.clone(), reviewer)?;
    Ok(Json(orchestrator.run(job).await?.record))
}

async fn save_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SaveConfigRequest>,
) -> Result<Json<SaveConfigResponse>, AppError> {
    require_auth(&state, &headers).await?;
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

async fn clear_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SaveConfigResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let mut config = state.config.lock().await;
    let keys = config.keys()?;
    let bind = config.bind;
    let relays = config.relays.clone();
    let auth_salt = config.auth_salt.clone();
    let auth_password_hash = config.auth_password_hash.clone();
    let auth_session_hash = config.auth_session_hash.clone();
    let mut reset = Config::new(keys)?;
    reset.bind = bind;
    reset.relays = relays;
    reset.auth_salt = auth_salt;
    reset.auth_password_hash = auth_password_hash;
    reset.auth_session_hash = auth_session_hash;
    reset.save(&state.project_root)?;
    *config = reset;
    *state.pending_tool.lock().await = None;
    Ok(Json(SaveConfigResponse {
        ok: true,
        profile_published: false,
    }))
}

async fn retire_bender(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SaveConfigResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let mut config = state.config.lock().await;
    let bind = config.bind;
    let relays = config.relays.clone();
    let auth_salt = config.auth_salt.clone();
    let auth_password_hash = config.auth_password_hash.clone();
    let auth_session_hash = config.auth_session_hash.clone();
    let mut retired = Config::new(nostr_sdk::prelude::Keys::generate())?;
    retired.bind = bind;
    retired.relays = relays;
    retired.auth_salt = auth_salt;
    retired.auth_password_hash = auth_password_hash;
    retired.auth_session_hash = auth_session_hash;
    retired.save(&state.project_root)?;
    *config = retired;
    *state.pending_tool.lock().await = None;
    chats::save(&state.project_root, &chats::ChatStore::default())?;
    Ok(Json(SaveConfigResponse {
        ok: true,
        profile_published: false,
    }))
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ModelsResponse>, AppError> {
    require_auth(&state, &headers).await?;
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

async fn new_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ChatResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let mut chat_store = chats::load(&state.project_root)?;
    let chat_id = chats::new_web_chat(&mut chat_store);
    chats::save(&state.project_root, &chat_store)?;
    Ok(Json(ChatResponse {
        chat_id: chat_id.clone(),
        chats: chats::summaries(&chat_store),
        messages: chats::messages(&chat_store, &chat_id),
    }))
}

async fn select_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SelectChatRequest>,
) -> Result<Json<ChatResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let mut chat_store = chats::load(&state.project_root)?;
    chats::set_active_web(&mut chat_store, &request.id)?;
    chats::save(&state.project_root, &chat_store)?;
    Ok(Json(ChatResponse {
        chat_id: request.id.clone(),
        chats: chats::summaries(&chat_store),
        messages: chats::messages(&chat_store, &request.id),
    }))
}

async fn add_tool_path(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AddToolPathRequest>,
) -> Result<Json<SaveConfigResponse>, AppError> {
    require_auth(&state, &headers).await?;
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
    let workspace = crate::workspace::Workspace::new(&state.project_root)?;
    if workspace.resolve_read(&path).is_err()
        || !path.starts_with(workspace.state_dir().join("tools"))
    {
        config.save(&state.project_root)?;
        return Err(
            anyhow::anyhow!("tool folders must be inside this workspace at .bender/tools").into(),
        );
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
        return Err(
            anyhow::anyhow!("tool path must contain bender-tool.toml or tool folders").into(),
        );
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

async fn pick_tool_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PickToolFolderResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("command -v zenity >/dev/null 2>&1 && zenity --file-selection --directory")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to open folder picker")?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "folder picker is not available; paste the folder path instead"
        )
        .into());
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(anyhow::anyhow!("no folder selected").into());
    }
    Ok(Json(PickToolFolderResponse { path }))
}

async fn ask(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AskRequest>,
) -> Result<Json<AskResponse>, AppError> {
    require_auth(&state, &headers).await?;
    let mut chat_store = chats::load(&state.project_root)?;
    let chat_id = match request.chat_id.as_deref() {
        Some(id) if chat_store.chats.iter().any(|chat| chat.id == id) => id.to_string(),
        _ => chats::ensure_web_chat(&mut chat_store),
    };
    let store = JobStore::new(&state.project_root)?;
    let instruction = request.instruction.trim();
    let (message, changed) = if instruction.eq_ignore_ascii_case("APPROVE") {
        let mut job = store
            .latest_awaiting_approval("web")?
            .ok_or_else(|| anyhow::anyhow!("no web job is awaiting approval"))?;
        append_web_conversation(&job, &chat_id, "web", "inbound", "APPROVE")?;
        job.approve()?;
        let project = ProjectConfig::load(&state.project_root)?;
        let reviewer: Option<SharedReviewer> = project
            .reviewers
            .get("gemma")
            .filter(|settings| settings.enabled)
            .map(|settings| {
                Arc::new(GemmaReviewer::new(&settings.base_url, &settings.model)) as SharedReviewer
            });
        let orchestrator =
            Orchestrator::new(&state.project_root, project, state.worker.clone(), reviewer)?;
        let job = orchestrator.run(job).await?;
        let response = if job.record.state == JobState::Complete {
            format!(
                "✓ Job complete\n\n{}",
                std::fs::read_to_string(job.path("final-report.md"))?
            )
        } else {
            format!(
                "Job {} stopped in {:?}: {}",
                job.record.id, job.record.state, job.record.message
            )
        };
        append_web_conversation(&job, &chat_id, "bender", "outbound", &response)?;
        (response, job.record.state == JobState::Complete)
    } else {
        if instruction.is_empty() {
            return Err(anyhow::anyhow!("task cannot be empty").into());
        }
        if let Some(mut job) = store.latest_in_state("web", JobState::Clarifying)? {
            let criteria = vec![
                AcceptanceCriterion {
                    id: "implementation".into(),
                    description: "The clarified request is implemented within this workspace."
                        .into(),
                    verified: false,
                },
                AcceptanceCriterion {
                    id: "checks".into(),
                    description: "All configured required checks pass.".into(),
                    verified: false,
                },
            ];
            let specification = format!(
                "# Proposed job specification\n\n## Original request\n\n{}\n\n## Clarification answers\n\n{instruction}\n\n## Acceptance criteria\n\n1. {}\n2. {}",
                job.request()?,
                criteria[0].description,
                criteria[1].description
            );
            job.set_specification(&specification, &criteria)?;
            append_web_conversation(&job, &chat_id, "web", "inbound", instruction)?;
            let response = format!(
                "Proposed acceptance criteria for {}:\n1. {}\n2. {}\n\nReply APPROVE to begin.",
                job.record.id, criteria[0].description, criteria[1].description
            );
            append_web_conversation(&job, &chat_id, "bender", "outbound", &response)?;
            (response, false)
        } else {
            let mut job = store.create(instruction, "web", Some(chat_id.clone()))?;
            job.transition(
                JobState::Clarifying,
                "Waiting for scope and acceptance clarification",
            )?;
            let attachment_note = if request.images.is_empty() {
                String::new()
            } else {
                format!(
                    "\n{} image attachment(s) were recorded but are not forwarded to Codex CLI yet.",
                    request.images.len()
                )
            };
            append_web_conversation(&job, &chat_id, "web", "inbound", instruction)?;
            let response = format!(
                    "Before I begin {}:\n1. What observable behavior proves this is complete?\n2. Are there compatibility, security, or scope constraints?\n3. Which configured checks are required?\n\nReply with the answers; I will persist a specification for approval.{}",
                    job.record.id, attachment_note
                );
            append_web_conversation(&job, &chat_id, "bender", "outbound", &response)?;
            (response, false)
        }
    };
    let user_message = if request.images.is_empty() {
        request.instruction.clone()
    } else {
        format!(
            "{}\n\n[{} image attachment(s): {}]",
            request.instruction,
            request.images.len(),
            request
                .images
                .iter()
                .map(|image| format!(
                    "{} ({}, {} bytes)",
                    image.name,
                    image.media_type,
                    image.data_url.len()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    chats::append(&mut chat_store, &chat_id, "user", &user_message)?;
    chats::update_title_from_user(&mut chat_store, &chat_id, &request.instruction);
    chats::append(&mut chat_store, &chat_id, "assistant", &message)?;
    chats::save(&state.project_root, &chat_store)?;

    Ok(Json(AskResponse {
        message,
        changed,
        pending_tool: None,
        chats: chats::summaries(&chat_store),
        messages: chats::messages(&chat_store, &chat_id),
        chat_id,
    }))
}

fn append_web_conversation(
    job: &crate::jobs::Job,
    conversation_id: &str,
    sender: &str,
    direction: &str,
    content: &str,
) -> Result<()> {
    append_jsonl(
        &job.path("conversation.jsonl"),
        &serde_json::json!({
            "timestamp": crate::jobs::now(),
            "conversation_id": conversation_id,
            "sender": sender,
            "direction": direction,
            "content": content
        }),
    )
}

async fn approve_tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ApproveToolRequest>,
) -> Result<Json<ApproveToolResponse>, AppError> {
    require_auth(&state, &headers).await?;
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
    let mut chat_store = chats::load(&state.project_root)?;
    let chat_id = pending
        .chat_id
        .clone()
        .unwrap_or_else(|| chats::ensure_web_chat(&mut chat_store));
    let tool_output = format!(
        "{}\n{}",
        message,
        serde_json::to_string_pretty(&result.output)?
    );
    chats::append(&mut chat_store, &chat_id, "assistant", &tool_output)?;
    chats::save(&state.project_root, &chat_store)?;
    Ok(Json(ApproveToolResponse {
        message,
        output: result.output,
        chats: chats::summaries(&chat_store),
        messages: chats::messages(&chat_store, &chat_id),
    }))
}

async fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let config = state.config.lock().await;
    if is_authenticated(&config, headers) {
        Ok(())
    } else {
        Err(AppError::with_status(
            anyhow::anyhow!("login required"),
            StatusCode::UNAUTHORIZED,
        ))
    }
}

fn is_authenticated(config: &Config, headers: &HeaderMap) -> bool {
    if config.auth_password_hash.is_none() {
        return false;
    }
    let Some(token) = cookie_value(headers, "bender_auth") else {
        return false;
    };
    config
        .auth_session_hash
        .as_deref()
        .is_some_and(|expected| session_hash(&token) == expected)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

fn auth_cookie(token: &str) -> String {
    format!("bender_auth={token}; Path=/; Max-Age=2592000; SameSite=Lax; HttpOnly")
}

fn random_hex() -> String {
    hex(&rand::random::<[u8; 32]>())
}

fn password_hash(salt: &str, password: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(salt.as_bytes());
    digest.update(password.as_bytes());
    let mut bytes = digest.finalize().to_vec();
    for _ in 0..100_000 {
        let mut digest = Sha256::new();
        digest.update(salt.as_bytes());
        digest.update(&bytes);
        bytes = digest.finalize().to_vec();
    }
    hex(&bytes)
}

fn session_hash(token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(token.as_bytes());
    hex(&digest.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(CHARS[(byte >> 4) as usize] as char);
        out.push(CHARS[(byte & 0x0f) as usize] as char);
    }
    out
}

struct AppError {
    error: anyhow::Error,
    status: StatusCode,
}

impl AppError {
    fn with_status(error: anyhow::Error, status: StatusCode) -> Self {
        Self { error, status }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            error: error.into(),
            status: StatusCode::BAD_REQUEST,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (self.status, self.error.to_string()).into_response()
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
    * { box-sizing: border-box; }
    body { margin: 0; background: #101827; color: #f4f7fb; overflow: hidden; }
    button, input, select, textarea { font: inherit; }
    button { border: 0; border-radius: 8px; padding: 10px 12px; background: #64b6e4; color: #07111e; font-weight: 700; cursor: pointer; }
    button.secondary { background: #273349; color: #eef6ff; }
    button.ghost { width: 100%; display: flex; justify-content: flex-start; background: transparent; color: #eef6ff; }
    button.ghost:hover, .chat-item:hover, .chat-item.is-active { background: #1c2b42; }
    button:disabled { opacity: .55; cursor: wait; }
    input, select, textarea { width: 100%; border: 1px solid #2d405d; border-radius: 8px; background: #111d30; color: #f4f7fb; }
    input, select { height: 38px; padding: 0 10px; margin: 6px 0 12px; }
    textarea { min-height: 52px; max-height: 150px; padding: 13px 16px; resize: vertical; background: #f4f0e6; color: #182234; border-color: #9eb2c6; }
    textarea::placeholder, input::placeholder { color: #73859d; }
    textarea:focus, input:focus, select:focus { outline: 2px solid #64b6e4; border-color: #64b6e4; }
    label { display: block; margin-top: 10px; color: #d5deea; font-size: 13px; font-weight: 700; }
    dl { margin: 0; }
    dt { margin-top: 12px; font-size: 12px; color: #9aa9bd; text-transform: uppercase; }
    dd { margin: 4px 0 0; overflow-wrap: anywhere; font-size: 14px; }
    .app { display: grid; grid-template-columns: 312px minmax(0, 1fr); height: 100vh; }
    .sidebar { display: flex; flex-direction: column; min-height: 0; height: 100vh; background: #0b1220; border-right: 1px solid #24344d; }
    .sidebar-top { padding: 18px 16px 12px; border-bottom: 1px solid #24344d; }
    .brand-logo { width: min(220px, 100%); max-height: 86px; object-fit: contain; display: block; }
    .settings-panel { border-bottom: 1px solid #24344d; }
    .settings-panel summary { list-style: none; cursor: pointer; padding: 14px 16px; color: #eef6ff; font-weight: 700; }
    .settings-panel summary::-webkit-details-marker { display: none; }
    .settings-panel summary::after { content: "›"; float: right; color: #64b6e4; transform: rotate(90deg); }
    .settings-panel[open] summary::after { transform: rotate(270deg); }
    .chat-list-wrap { min-height: 0; padding: 12px 8px; border-bottom: 1px solid #24344d; }
    .chat-list-title { margin: 12px 8px 8px; color: #9aa9bd; font-size: 12px; text-transform: uppercase; }
    .chat-list { max-height: 226px; overflow-y: auto; padding-right: 3px; }
    .chat-item { width: 100%; display: block; margin: 2px 0; padding: 10px 12px; border-radius: 8px; color: #eef6ff; background: transparent; text-align: left; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .settings { max-height: min(58vh, 560px); overflow-y: auto; padding: 0 16px 18px; }
    .main { min-width: 0; min-height: 0; height: 100vh; display: grid; grid-template-rows: auto minmax(0, 1fr) auto; background: #101827; }
    .topbar { min-height: 88px; display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 20px 28px; border-bottom: 1px solid #24344d; }
    .project { color: #9aa9bd; overflow-wrap: anywhere; text-align: right; }
    .tool-badges { display: flex; flex-wrap: wrap; justify-content: flex-end; gap: 6px; }
    .tool-badge { border: 1px solid #64b6e4; border-radius: 999px; padding: 4px 9px; background: #142238; color: #dff3ff; font-size: 12px; font-weight: 700; }
    .messages { min-height: 0; overflow-y: auto; padding: 28px max(28px, calc((100vw - 1160px) / 2)); }
    .message { display: flex; margin: 0 0 18px; }
    .message.user { justify-content: flex-end; }
    .bubble { max-width: min(760px, 88%); padding: 14px 16px; border-radius: 18px; line-height: 1.5; white-space: pre-wrap; overflow-wrap: anywhere; }
    .message.user .bubble { background: #1e5a7b; color: #f4fbff; border-bottom-right-radius: 6px; }
    .message.assistant .bubble, .message.system .bubble { background: #f4f0e6; color: #182234; border-bottom-left-radius: 6px; }
    .composer { padding: 14px max(28px, calc((100vw - 1160px) / 2)) 16px; border-top: 1px solid #24344d; background: #101827; }
    .composer-inner { max-width: 920px; margin: 0 auto; }
    .composer-row { display: grid; grid-template-columns: auto minmax(0, 1fr) auto; gap: 10px; align-items: end; }
    .attach-button { width: 44px; height: 44px; padding: 0; border-radius: 999px; background: #273349; color: #eef6ff; }
    .send-floating { width: 44px; height: 44px; padding: 0; border-radius: 999px; }
    .image-input { display: none; }
    .attachment-preview { max-width: 920px; margin: 0 auto 10px; display: flex; flex-wrap: wrap; gap: 8px; }
    .attachment-item { position: relative; width: 72px; height: 72px; border: 1px solid #2d405d; border-radius: 8px; overflow: hidden; background: #111d30; }
    .attachment-item img { width: 100%; height: 100%; object-fit: cover; display: block; }
    .attachment-remove { position: absolute; top: 4px; right: 4px; width: 22px; height: 22px; padding: 0; border-radius: 999px; background: rgba(7, 17, 30, .88); color: #f4f7fb; font-size: 14px; line-height: 1; }
    .status-line { min-height: 22px; margin-top: 8px; color: #9aa9bd; font-size: 13px; }
    .warning { display: none; margin-bottom: 10px; padding: 10px 12px; border: 1px solid #b57d20; border-radius: 8px; background: #2d2106; color: #ffd887; font-size: 14px; }
    .tool-approval { display: none; max-width: 920px; margin: 0 auto 12px; padding: 12px; border: 1px solid #64b6e4; border-radius: 8px; background: #111d30; color: #f4f7fb; }
    .tool-approval strong { display: block; margin-bottom: 6px; }
    .tool-approval code { display: block; margin: 8px 0; white-space: pre-wrap; overflow-wrap: anywhere; color: #d5deea; font-size: 12px; }
    .tool-path-actions { display: grid; grid-template-columns: 1fr auto; gap: 8px; align-items: center; }
    .settings-actions { display: flex; flex-wrap: wrap; gap: 8px; margin: 0 0 4px; }
    .danger-button { background: #4b2634; color: #ffd8e3; }
    .provider-field { display: none; }
    .provider-field.is-visible { display: block; }
    .copy-value { cursor: pointer; border-radius: 6px; padding: 4px 6px; margin-left: -6px; }
    .copy-value:hover { background: #1c2b42; color: #ffffff; }
    .login-screen { position: fixed; inset: 0; z-index: 100; display: grid; place-items: center; padding: 24px; background: #101827; }
    .login-panel { width: min(420px, 100%); padding: 22px; border: 1px solid #24344d; border-radius: 8px; background: #0b1220; }
    .login-panel img { width: min(240px, 100%); display: block; margin: 0 0 18px; }
    .login-panel h1 { margin: 0 0 12px; font-size: 20px; letter-spacing: 0; }
    .login-panel p { margin: 0 0 14px; color: #9aa9bd; }
    .login-screen.is-hidden { display: none; }
    .mobile-menu-button, .drawer-backdrop { display: none; }
    @media (max-width: 860px) {
      .app { grid-template-columns: 1fr; }
      body { overflow: hidden; }
      .sidebar { position: fixed; inset: 0 auto 0 0; z-index: 40; width: min(330px, 86vw); transform: translateX(-102%); transition: transform .18s ease; box-shadow: 18px 0 40px rgba(0,0,0,.35); }
      body.drawer-open .sidebar { transform: translateX(0); }
      .drawer-backdrop { position: fixed; inset: 0; z-index: 30; display: none; border: 0; border-radius: 0; padding: 0; background: rgba(4, 9, 17, .62); }
      body.drawer-open .drawer-backdrop { display: block; }
      .main { height: 100vh; min-height: 0; grid-template-rows: auto minmax(0, 1fr) auto; }
      .settings { max-height: 58vh; }
      .topbar { min-height: 74px; align-items: flex-start; flex-direction: column; padding: 12px 16px 12px 64px; }
      .mobile-menu-button { position: fixed; top: 14px; left: 14px; z-index: 20; display: inline-grid; place-items: center; width: 38px; height: 38px; padding: 0; border: 1px solid #2d405d; border-radius: 8px; background: #111d30; color: #f4f7fb; font-size: 22px; line-height: 1; }
      body.drawer-open .mobile-menu-button { left: min(calc(86vw - 52px), 278px); z-index: 50; }
      .project { text-align: left; }
      .tool-badges { justify-content: flex-start; }
      .messages { padding: 18px 14px; }
      .bubble { max-width: 92%; }
      .composer { padding: 12px 12px 14px; }
    }
  </style>
</head>
<body>
  <div id="loginScreen" class="login-screen">
    <form id="loginForm" class="login-panel">
      <img src="/logo.png" alt="Bender">
      <h1 id="loginTitle">Unlock Bender</h1>
      <p id="loginHelp">Enter your password to continue.</p>
      <label for="loginPassword">Password</label>
      <input id="loginPassword" type="password" autocomplete="current-password" />
      <button id="loginButton" type="submit">Continue</button>
      <div id="loginError" class="status-line"></div>
    </form>
  </div>
  <button id="menuButton" class="mobile-menu-button" title="Open menu">☰</button>
  <button id="drawerBackdrop" class="drawer-backdrop" title="Close menu"></button>
  <div class="app">
    <aside class="sidebar">
      <div class="sidebar-top">
        <img class="brand-logo" src="/logo.png" alt="Bender">
      </div>
      <details class="settings-panel">
        <summary>Settings</summary>
        <div class="settings">
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
          <div class="settings-actions">
            <button id="saveConfig">Save Setup</button>
            <button class="secondary" id="clearConfig">Clear Settings</button>
            <button class="danger-button" id="retireBender">Retire Bender</button>
          </div>
          <label for="toolPathInput">Tool folder</label>
          <div class="tool-path-actions">
            <input id="toolPathInput" spellcheck="false" placeholder="/path/to/tool-or-tools-folder" />
            <button class="secondary" id="chooseToolPath">Choose</button>
          </div>
          <button class="secondary" id="addToolPath">Add Tool Folder</button>
          <dl>
            <dt>tool paths</dt><dd id="toolPaths"></dd>
            <dt>npub</dt><dd id="npub" class="copy-value" title="Copy npub"></dd>
            <dt>version</dt><dd id="version"></dd>
            <dt>auth</dt><dd id="auth"></dd>
            <dt>relays</dt><dd id="relays"></dd>
          </dl>
        </div>
      </details>
      <div class="chat-list-wrap">
        <div class="chat-list-title">Jobs</div>
        <div id="jobList" class="chat-list"></div>
        <button class="ghost" id="newChat">New chat</button>
        <div class="chat-list-title">Chats</div>
        <div id="chatList" class="chat-list"></div>
      </div>
    </aside>
    <main class="main">
      <header class="topbar">
        <div id="warning" class="warning"></div>
        <div>
          <div id="headerTools" class="tool-badges"></div>
          <div id="project" class="project"></div>
        </div>
      </header>
      <section id="messages" class="messages"></section>
      <footer class="composer">
        <div id="toolApproval" class="tool-approval">
          <strong id="toolTitle"></strong>
          <div id="toolPermissions"></div>
          <code id="toolInput"></code>
          <button id="approveTool">Approve Tool</button>
        </div>
        <div class="composer-inner">
          <div id="attachmentPreview" class="attachment-preview"></div>
          <div class="composer-row">
            <button id="attachImage" class="attach-button" title="Attach image">＋</button>
            <input id="imageInput" class="image-input" type="file" accept="image/*" multiple />
            <textarea id="instruction" placeholder="Describe a software job, or reply APPROVE..."></textarea>
            <button id="send" class="send-floating" title="Send">↑</button>
          </div>
          <div id="statusLine" class="status-line"></div>
        </div>
      </footer>
    </main>
  </div>
  <script>
    const $ = id => document.getElementById(id);
    let pendingTool = null;
    let activeChatId = null;
    let attachedImages = [];
    const escapeHtml = value => String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
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
    async function checkAuth() {
      const auth = await api('/api/auth/status');
      if (auth.authenticated) {
        $('loginScreen').classList.add('is-hidden');
        localStorage.setItem('bender_auth_ready', '1');
        await load();
        return;
      }
      $('loginScreen').classList.remove('is-hidden');
      $('loginTitle').textContent = auth.configured ? 'Unlock Bender' : 'Create Bender Password';
      $('loginHelp').textContent = auth.configured
        ? 'Enter your password to continue.'
        : 'Choose a password for this local Bender. It will not be stored as plain text.';
      $('loginPassword').autocomplete = auth.configured ? 'current-password' : 'new-password';
      $('loginPassword').focus();
    }
    async function load() {
      const status = await api('/api/status');
      document.title = `Bender ${status.version}`;
      activeChatId = status.active_chat_id;
      $('project').textContent = status.project_root;
      $('warning').style.display = status.project_warning ? 'block' : 'none';
      $('warning').textContent = status.project_warning || '';
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
      renderJobs(status.jobs);
      renderChats(status.chats);
      renderMessages(status.messages);
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
    function renderChats(chats) {
      $('chatList').innerHTML = chats.map(chat => `
        <button class="chat-item ${chat.id === activeChatId ? 'is-active' : ''}" data-chat-id="${chat.id}" title="${escapeHtml(chat.title)}">${escapeHtml(chat.title)}</button>
      `).join('');
      document.querySelectorAll('[data-chat-id]').forEach(item => {
        item.onclick = async () => {
          const result = await api('/api/chats/select', { id: item.dataset.chatId });
          activeChatId = result.chat_id;
          renderChats(result.chats);
          renderMessages(result.messages);
          closeDrawerOnMobile();
        };
      });
    }
    function renderJobs(jobs) {
      $('jobList').innerHTML = jobs.length
        ? jobs.map(job => `<div class="chat-item" title="${escapeHtml(job.message)}"><strong>${escapeHtml(job.state)}</strong> · attempt ${job.attempt}<br><small>${escapeHtml(job.id)}</small></div>`).join('')
        : '<div class="chat-item">No jobs yet</div>';
    }
    function renderMessages(messages) {
      $('messages').innerHTML = messages.length
        ? messages.map(message => `<div class="message ${message.role}"><div class="bubble">${escapeHtml(message.content)}</div></div>`).join('')
        : '<div class="message assistant"><div class="bubble">Ready.</div></div>';
      $('messages').scrollTop = $('messages').scrollHeight;
    }
    function appendTemporary(role, content) {
      const node = document.createElement('div');
      node.className = `message ${role}`;
      node.innerHTML = `<div class="bubble">${escapeHtml(content)}</div>`;
      $('messages').appendChild(node);
      $('messages').scrollTop = $('messages').scrollHeight;
    }
    async function copyText(value, copiedMessage) {
      if (!value) return;
      await navigator.clipboard.writeText(value);
      $('statusLine').textContent = copiedMessage;
    }
    function setDrawer(open) {
      document.body.classList.toggle('drawer-open', open);
      $('menuButton').textContent = open ? '×' : '☰';
      $('menuButton').title = open ? 'Close menu' : 'Open menu';
    }
    function closeDrawerOnMobile() {
      if (window.matchMedia('(max-width: 860px)').matches) setDrawer(false);
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
        ? tools.map(tool => `<span class="tool-badge" title="${escapeHtml(tool.description)}">${escapeHtml(tool.name)}</span>`).join('')
        : '';
      $('toolPaths').textContent = toolPaths.length ? toolPaths.map(toolPath => toolPath.path).join(', ') : 'none';
    }
    function renderAttachments() {
      $('attachmentPreview').innerHTML = attachedImages.map((image, index) => `
        <div class="attachment-item" title="${escapeHtml(image.name)}">
          <img src="${image.data_url}" alt="${escapeHtml(image.name)}">
          <button class="attachment-remove" data-image-index="${index}" title="Remove image">×</button>
        </div>
      `).join('');
      document.querySelectorAll('[data-image-index]').forEach(button => {
        button.onclick = () => {
          attachedImages.splice(Number(button.dataset.imageIndex), 1);
          renderAttachments();
        };
      });
    }
    function readImage(file) {
      return new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve({
          name: file.name,
          media_type: file.type || 'image/png',
          data_url: String(reader.result)
        });
        reader.onerror = () => reject(new Error(`Could not read ${file.name}`));
        reader.readAsDataURL(file);
      });
    }
    $('loginForm').onsubmit = async event => {
      event.preventDefault();
      $('loginButton').disabled = true;
      $('loginError').textContent = '';
      $('loginButton').textContent = 'Checking...';
      try {
        await api('/api/auth/login', { password: $('loginPassword').value });
        $('loginPassword').value = '';
        localStorage.setItem('bender_auth_ready', '1');
        $('loginScreen').classList.add('is-hidden');
        await load();
      } catch (err) {
        $('loginError').textContent = err.message;
      } finally {
        $('loginButton').disabled = false;
        $('loginButton').textContent = 'Continue';
      }
    };
    $('newChat').onclick = async () => {
      const result = await api('/api/chats/new', {});
      activeChatId = result.chat_id;
      renderChats(result.chats);
      renderMessages(result.messages);
      closeDrawerOnMobile();
      $('instruction').focus();
    };
    $('menuButton').onclick = () => setDrawer(!document.body.classList.contains('drawer-open'));
    $('drawerBackdrop').onclick = () => setDrawer(false);
    $('attachImage').onclick = () => $('imageInput').click();
    $('imageInput').onchange = async () => {
      const files = Array.from($('imageInput').files || []);
      $('imageInput').value = '';
      try {
        for (const file of files) {
          if (!file.type.startsWith('image/')) continue;
          if (file.size > 4 * 1024 * 1024) throw new Error(`${file.name} is larger than 4 MB`);
          if (attachedImages.length >= 3) throw new Error('Attach up to 3 images per message');
          attachedImages.push(await readImage(file));
        }
        renderAttachments();
      } catch (err) {
        $('statusLine').textContent = err.message;
      }
    };
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
        $('statusLine').textContent = result.profile_published
          ? 'I published my Nostr profile! Bite my shiny metal ass.'
          : 'Setup saved, but I could not publish my Nostr profile.';
      } catch (err) {
        await load();
        $('statusLine').textContent = err.message;
      } finally {
        $('saveConfig').disabled = false;
      }
    };
    $('clearConfig').onclick = async () => {
      if (!confirm('Clear provider keys, controller npub, selected provider/model, and tool folders?')) return;
      $('clearConfig').disabled = true;
      try {
        await api('/api/config/clear', {});
        $('apiKey').value = '';
        $('anthropicApiKey').value = '';
        $('deepseekApiKey').value = '';
        $('llamaCppApiKey').value = '';
        $('toolPathInput').value = '';
        await load();
        $('statusLine').textContent = 'Settings cleared.';
      } catch (err) {
        $('statusLine').textContent = err.message;
      } finally {
        $('clearConfig').disabled = false;
      }
    };
    $('retireBender').onclick = async () => {
      if (!confirm('Retire this Bender and create a fresh identity? This replaces the npub/nsec and clears chats/settings.')) return;
      $('retireBender').disabled = true;
      try {
        await api('/api/config/retire', {});
        $('apiKey').value = '';
        $('anthropicApiKey').value = '';
        $('deepseekApiKey').value = '';
        $('llamaCppApiKey').value = '';
        $('toolPathInput').value = '';
        await load();
        $('statusLine').textContent = 'Bender retired. A fresh Bender is ready.';
      } catch (err) {
        $('statusLine').textContent = err.message;
      } finally {
        $('retireBender').disabled = false;
      }
    };
    $('refreshModels').onclick = async () => {
      $('refreshModels').disabled = true;
      try {
        const current = $('model').value;
        const result = await api('/api/models');
        setModels(result.models);
        if (result.models.includes(current)) $('model').value = current;
        $('statusLine').textContent = 'Model list refreshed.';
      } catch (err) {
        $('statusLine').textContent = err.message;
      } finally {
        $('refreshModels').disabled = false;
      }
    };
    $('chooseToolPath').onclick = async () => {
      try {
        const result = await api('/api/tools/pick-folder', {});
        $('toolPathInput').value = result.path;
      } catch (err) {
        $('statusLine').textContent = err.message;
      }
    };
    $('addToolPath').onclick = async () => {
      $('addToolPath').disabled = true;
      try {
        await api('/api/tools/path', { path: $('toolPathInput').value });
        $('toolPathInput').value = '';
        await load();
        $('statusLine').textContent = 'Tool folder added.';
      } catch (err) {
        $('statusLine').textContent = err.message;
        await load();
      } finally {
        $('addToolPath').disabled = false;
      }
    };
    async function sendMessage() {
      const instruction = $('instruction').value.trim();
      const images = attachedImages.slice();
      if (!instruction && !images.length) return;
      $('send').disabled = true;
      $('instruction').value = '';
      attachedImages = [];
      renderAttachments();
      $('statusLine').textContent = 'Thinking...';
      appendTemporary('user', images.length
        ? `${instruction}${instruction ? '\n\n' : ''}Attached images:\n${images.map(image => `- ${image.name} (${image.media_type})`).join('\n')}`
        : instruction);
      try {
        const result = await api('/api/ask', { chat_id: activeChatId, instruction, images });
        activeChatId = result.chat_id;
        showPendingTool(result.pending_tool);
        renderChats(result.chats);
        renderMessages(result.messages);
        $('statusLine').textContent = result.changed ? 'Changed files.' : '';
      } catch (err) {
        appendTemporary('assistant', err.message);
        $('statusLine').textContent = '';
      } finally {
        $('send').disabled = false;
        $('instruction').focus();
      }
    }
    $('send').onclick = sendMessage;
    $('instruction').addEventListener('keydown', event => {
      if (event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault();
        sendMessage();
      }
    });
    $('approveTool').onclick = async () => {
      if (!pendingTool) return;
      $('approveTool').disabled = true;
      try {
        const result = await api('/api/tools/approve', { id: pendingTool.id });
        showPendingTool(null);
        renderChats(result.chats);
        renderMessages(result.messages);
        $('statusLine').textContent = result.message;
      } catch (err) {
        $('statusLine').textContent = err.message;
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
    checkAuth().catch(err => $('loginError').textContent = err.message);
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
        "This looks like a Cargo build-output folder. Bender controls its launch directory; cd to the intended project and run the globally installed bender binary there.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_ui_exposes_jobs_approval_and_loopback_defaults() {
        assert!(INDEX.contains("Jobs"));
        assert!(INDEX.contains("reply APPROVE"));
        assert!(INDEX.contains("jobList"));
        assert!(crate::config::Config::new(nostr_sdk::Keys::generate())
            .unwrap()
            .bind
            .ip()
            .is_loopback());
    }
}
