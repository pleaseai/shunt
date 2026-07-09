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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Run { config }) => run(config.or(cli.config)).await,
        Some(Command::Check { config }) => check(config.or(cli.config)),
        None if cli.check => check(cli.config),
        None => run(cli.config).await,
    }
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
    axum::serve(listener, server::build_router(config)).await?;
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
        .with(tracing_subscriber::fmt::layer())
        .init();
}
