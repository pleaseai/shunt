use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use shunt::{config::Config, server};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Debug, Parser)]
#[command(name = "shunt", about = "Claude Code LLM gateway")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[arg(long)]
    check: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Run {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Check {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print a Claude subscription OAuth token to stdout, for use as an
    /// `apiKeyHelper`. Static mode echoes `SHUNT_GATEWAY_TOKEN` /
    /// `CLAUDE_CODE_OAUTH_TOKEN`; otherwise auto-refresh mode reads and refreshes
    /// `~/.claude/.credentials.json`.
    Token,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Run { config }) => run(config.or(cli.config)).await,
        Some(Command::Check { config }) => check(config.or(cli.config)),
        Some(Command::Token) => token().await,
        None if cli.check => check(cli.config),
        None => run(cli.config).await,
    }
}

async fn token() -> anyhow::Result<()> {
    let path = shunt::auth::claude_auth::default_credentials_path();
    let client = reqwest::Client::new();
    // stdout carries only the token so it can be consumed by apiKeyHelper.
    let token = shunt::auth::claude_auth::resolve_token(path, client).await?;
    println!("{token}");
    Ok(())
}

async fn run(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config = Config::load(config_path.as_deref()).context("failed to load config")?;
    let bind = config
        .server
        .bind_addr()
        .context("invalid server bind address")?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read bind address")?;
    tracing::info!(%local_addr, "shunt listening");
    let router = server::build_router(config).context("failed to initialize gateway")?;
    axum::serve(listener, router).await?;
    Ok(())
}

fn check(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    Config::load(config_path.as_deref())
        .and_then(|config| config.validate())
        .context("config check failed")?;
    println!("config ok");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("shunt=info"));
    tracing_subscriber::registry()
        .with(filter)
        // Logs go to stderr so command stdout (e.g. the `token` subcommand's
        // apiKeyHelper output) stays free of log noise.
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}
