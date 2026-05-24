mod chats;
mod config;
mod nostr_agent;
mod patch;
mod project;
mod providers;
mod tools;
mod web;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nostr_sdk::prelude::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;

#[derive(Debug, Parser)]
#[command(name = "bender")]
#[command(about = "Bender: a Nostr-controlled local coding agent for one project folder")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Run {
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_api_key: Option<String>,
        #[arg(long, env = "ANTHROPIC_API_KEY")]
        anthropic_api_key: Option<String>,
        #[arg(long, env = "DEEPSEEK_API_KEY")]
        deepseek_api_key: Option<String>,
        #[arg(
            long,
            env = "BENDER_BIND",
            help = "Local web UI bind address, e.g. 127.0.0.1:7332"
        )]
        bind: Option<std::net::SocketAddr>,
    },
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("bender=info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let project_root = std::env::current_dir().context("could not read current directory")?;

    match cli.command {
        Command::Init => init(project_root).await,
        Command::Run {
            openai_api_key,
            anthropic_api_key,
            deepseek_api_key,
            bind,
        } => {
            run(
                project_root,
                openai_api_key,
                anthropic_api_key,
                deepseek_api_key,
                bind,
            )
            .await
        }
        Command::Status => status(project_root),
    }
}

async fn init(project_root: PathBuf) -> Result<()> {
    warn_if_build_output_dir(&project_root);
    let config_path = Config::path(&project_root);
    if config_path.exists() {
        anyhow::bail!("{} already exists", config_path.display());
    }

    let keys = Keys::generate();
    let config = Config::new(keys)?;
    config.save(&project_root)?;

    println!("Bender initialized.");
    println!("Config: {}", config_path.display());
    println!("npub: {}", config.public_key);
    println!("nsec: {}", config.secret_key);
    println!();
    println!("Start the daemon and use the local web UI to finish setup:");
    println!("  bender run");
    Ok(())
}

async fn run(
    project_root: PathBuf,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    deepseek_api_key: Option<String>,
    bind: Option<std::net::SocketAddr>,
) -> Result<()> {
    warn_if_build_output_dir(&project_root);
    let mut config = Config::load(&project_root)?;
    if let Some(api_key) = openai_api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok()) {
        config.openai_api_key = Some(api_key);
    }
    if let Some(api_key) = anthropic_api_key.or_else(|| std::env::var("ANTHROPIC_API_KEY").ok()) {
        config.anthropic_api_key = Some(api_key);
    }
    if let Some(api_key) = deepseek_api_key.or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()) {
        config.deepseek_api_key = Some(api_key);
    }
    if let Some(bind) = bind {
        config.bind = bind;
    }

    let bind = config.bind;
    let state = web::AppState::new(project_root, config);
    let listener = web::bind(&state).await?;
    let nostr_state = state.clone();

    tokio::spawn(async move {
        if let Err(err) = nostr_agent::run(nostr_state).await {
            tracing::warn!(error = %err, "nostr listener stopped");
        }
    });

    println!("Bender running at {}", local_web_url(bind));
    println!("Bound to http://{bind}");
    web::serve(state, listener).await
}

fn local_web_url(bind: std::net::SocketAddr) -> String {
    if bind.ip().is_loopback() {
        format!("http://bender.localhost:{}", bind.port())
    } else {
        format!("http://{bind}")
    }
}

fn warn_if_build_output_dir(project_root: &std::path::Path) {
    if looks_like_cargo_output_dir(project_root) {
        eprintln!();
        eprintln!(
            "Warning: Bender is running from {}, which looks like a Cargo build output folder.",
            project_root.display()
        );
        eprintln!("Bender controls the current working directory.");
        eprintln!("For normal use, copy the bender binary into the project folder you want controlled, cd there, then run ./bender.");
        eprintln!();
    }
}

fn looks_like_cargo_output_dir(path: &std::path::Path) -> bool {
    let components: Vec<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    components
        .windows(2)
        .any(|window| window == ["target", "release"] || window == ["target", "debug"])
}

fn status(project_root: PathBuf) -> Result<()> {
    let config = Config::load(&project_root)?;
    println!("project: {}", project_root.display());
    println!("config: {}", Config::path(&project_root).display());
    println!("name: {}", config.name);
    println!("npub: {}", config.public_key);
    println!(
        "controller: {}",
        config
            .controller_npub
            .as_deref()
            .unwrap_or("not configured")
    );
    println!("provider: {}", config.provider.label());
    println!("model: {}", config.model);
    println!("bind: {}", config.bind);
    println!("relays:");
    for relay in &config.relays {
        println!("  {relay}");
    }
    Ok(())
}
