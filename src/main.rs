use std::net::SocketAddr;

use anyhow::Result;
use bender::{
    config::Config, doctor, jobs::JobStore, nostr_agent, project_config::ProjectConfig, web,
    workspace::Workspace,
};
use clap::{Parser, Subcommand};
use nostr_sdk::prelude::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Debug, Parser)]
#[command(name = "bender", version)]
#[command(about = "Folder-scoped autonomous software-job orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Run {
        #[arg(
            long,
            env = "BENDER_BIND",
            help = "Local web UI bind address, e.g. 127.0.0.1:7332"
        )]
        bind: Option<SocketAddr>,
    },
    Setup {
        #[arg(long, help = "Save detected commands as this project's approved argv")]
        accept_detected: bool,
    },
    Doctor {
        #[arg(
            long,
            help = "Run a harmless authenticated Codex invocation in read-only mode"
        )]
        codex_smoke_test: bool,
    },
    Jobs,
    Models,
    Workers,
    Status,
    Version,
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("bender=info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let workspace = Workspace::current()?;
    match cli.command.unwrap_or(Command::Run { bind: None }) {
        Command::Init => init(&workspace, false),
        Command::Run { bind } => run(workspace, bind).await,
        Command::Setup { accept_detected } => setup(&workspace, accept_detected),
        Command::Doctor { codex_smoke_test } => doctor_command(&workspace, codex_smoke_test),
        Command::Jobs => jobs(&workspace),
        Command::Models => models(&workspace),
        Command::Workers => workers(&workspace),
        Command::Status => status(&workspace),
        Command::Version => {
            println!("bender {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Update => {
            println!("Bender does not update itself without an explicit installer action.");
            println!(
                "To install the latest maintainer release:\n  curl -fsSL https://raw.githubusercontent.com/lnbits/bender/main/install.sh | sh"
            );
            Ok(())
        }
    }
}

fn init(workspace: &Workspace, quiet: bool) -> Result<()> {
    workspace.initialize()?;
    let config_path = Config::path(workspace.root());
    if !config_path.exists() {
        let config = Config::new(Keys::generate())?;
        config.save(workspace.root())?;
    }
    let project_path = ProjectConfig::path(workspace.root());
    if !project_path.exists() {
        ProjectConfig::default().save(workspace.root())?;
    }
    JobStore::new(workspace.root())?;
    std::fs::create_dir_all(workspace.state_dir().join("artifacts"))?;
    if !quiet {
        let config = Config::load(workspace.root())?;
        println!("Bender initialized for {}", workspace.root().display());
        println!("Config: {}", config_path.display());
        println!("Project policy: {}", project_path.display());
        println!("Nostr npub: {}", config.public_key);
        println!("No secret credentials were printed.");
        println!("\nNext:\n  bender setup\n  bender doctor\n  bender");
    }
    Ok(())
}

async fn run(workspace: Workspace, bind: Option<SocketAddr>) -> Result<()> {
    init(&workspace, true)?;
    let mut config = Config::load(workspace.root())?;
    if let Some(bind) = bind {
        config.bind = bind;
    }
    let store = JobStore::new(workspace.root())?;
    for id in store.recover_interrupted()? {
        tracing::warn!(job_id = %id, "recovered interrupted job as blocked");
    }

    let bind = config.bind;
    let state = web::AppState::new(workspace.root().to_path_buf(), config.clone());
    let listener = web::bind(&state).await?;
    if config.controller()?.is_some() && !config.relays.is_empty() {
        let nostr_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = nostr_agent::run(nostr_state).await {
                tracing::warn!(%error, "Nostr listener stopped");
            }
        });
    } else {
        tracing::info!("Nostr listener disabled until a controller is configured");
    }

    println!("Workspace: {}", workspace.root().display());
    println!("Bender running at {}", local_web_url(bind));
    println!("Bound to http://{bind}");
    web::serve(state, listener).await
}

fn setup(workspace: &Workspace, accept_detected: bool) -> Result<()> {
    init(workspace, true)?;
    let detected = ProjectConfig::detected(workspace.root());
    if accept_detected {
        detected.save(workspace.root())?;
        println!(
            "Approved detected argv commands in {}",
            ProjectConfig::path(workspace.root()).display()
        );
    } else {
        println!("Detected project policy proposal (not approved or saved):\n");
        println!("{}", toml::to_string_pretty(&detected)?);
        println!("Review it, then run `bender setup --accept-detected` or edit:");
        println!("  {}", ProjectConfig::path(workspace.root()).display());
    }
    Ok(())
}

fn doctor_command(workspace: &Workspace, codex_smoke_test: bool) -> Result<()> {
    init(workspace, true)?;
    let checks = doctor::run(workspace, codex_smoke_test);
    for check in &checks {
        let mark = if check.ok {
            "✓"
        } else if check.required {
            "✗"
        } else {
            "○"
        };
        println!("{mark} {} — {}", check.name, check.detail);
    }
    if checks.iter().any(|check| check.required && !check.ok) {
        anyhow::bail!("one or more required doctor checks failed");
    }
    Ok(())
}

fn jobs(workspace: &Workspace) -> Result<()> {
    init(workspace, true)?;
    let jobs = JobStore::new(workspace.root())?.list()?;
    if jobs.is_empty() {
        println!("No jobs in {}", workspace.root().display());
    }
    for job in jobs {
        println!(
            "{}\t{:?}\tattempt {}\t{}",
            job.id, job.state, job.attempt, job.message
        );
    }
    Ok(())
}

fn models(workspace: &Workspace) -> Result<()> {
    let project = ProjectConfig::load(workspace.root())?;
    println!("Primary: Codex CLI (model selected by Codex configuration)");
    for (name, worker) in project.workers {
        println!(
            "Worker {name}: {} via {} ({})",
            worker.model, worker.provider, worker.base_url
        );
    }
    for (name, reviewer) in project.reviewers {
        println!(
            "Reviewer {name}: {} via {} ({})",
            reviewer.model, reviewer.provider, reviewer.base_url
        );
    }
    Ok(())
}

fn workers(workspace: &Workspace) -> Result<()> {
    println!(
        "codex_cli\tprimary\t{}",
        command_version("codex", &["--version"])
    );
    let project = ProjectConfig::load(workspace.root())?;
    for (name, worker) in project.workers {
        println!(
            "{name}\t{}\t{}",
            if worker.enabled {
                "enabled"
            } else {
                "disabled"
            },
            worker.model
        );
    }
    Ok(())
}

fn status(workspace: &Workspace) -> Result<()> {
    let config = Config::load(workspace.root())?;
    println!("workspace: {}", workspace.root().display());
    println!("config: {}", Config::path(workspace.root()).display());
    println!("npub: {}", config.public_key);
    println!(
        "controller: {}",
        config
            .controller_npub
            .as_deref()
            .unwrap_or("not configured")
    );
    println!("bind: {}", config.bind);
    println!("jobs: {}", JobStore::new(workspace.root())?.list()?.len());
    Ok(())
}

fn command_version(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|error| format!("unavailable: {error}"))
}

fn local_web_url(bind: SocketAddr) -> String {
    if bind.ip().is_loopback() {
        format!("http://bender.localhost:{}", bind.port())
    } else {
        format!("http://{bind}")
    }
}
