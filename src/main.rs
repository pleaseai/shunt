use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use shunt::{
    config::{Config, SentryConfig},
    server,
};
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
    /// Log in to a subscription provider via its OAuth device-code flow and save
    /// the credential for shunt to inject. Currently supports `xai` (SuperGrok /
    /// X Premium+): `shunt login xai`.
    Login {
        /// Provider to log in to (currently: `xai`).
        provider: String,
    },
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Run { config }) => run(config.or(cli.config)),
        Some(Command::Check { config }) => check(config.or(cli.config)),
        Some(Command::Token) => runtime()?.block_on(token()),
        Some(Command::Login { provider }) => {
            runtime()?.block_on(shunt::auth::xai_login::run(&provider))
        }
        None if cli.check => check(cli.config),
        None => run(cli.config),
    }
}

/// The runtime is built by hand (not `#[tokio::main]`) so `run` can initialize
/// Sentry before any runtime thread exists, per sentry-rust guidance.
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start tokio runtime")
}

async fn token() -> anyhow::Result<()> {
    let path = shunt::auth::claude_auth::default_credentials_path();
    let client = reqwest::Client::new();
    // stdout carries only the token so it can be consumed by apiKeyHelper.
    let token = shunt::auth::claude_auth::resolve_token(path, client).await?;
    println!("{token}");
    Ok(())
}

fn run(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config = Config::load(config_path.as_deref()).context("failed to load config")?;
    // The guard must outlive the runtime so buffered events flush on shutdown.
    let _sentry = init_sentry(config.sentry.as_ref());
    runtime()?.block_on(serve(config))
}

async fn serve(config: Config) -> anyhow::Result<()> {
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

/// Opt-in Sentry error reporting: a client exists only when the operator
/// configured a non-empty `[sentry] dsn`, and it reports gateway-owned
/// diagnostics only — panics and `error!` events, never request/response
/// bodies, headers, or credentials.
fn init_sentry(config: Option<&SentryConfig>) -> Option<sentry::ClientInitGuard> {
    let config = config.filter(|sentry| sentry.enabled())?;
    let guard = sentry::init(sentry::ClientOptions {
        // Validated at config load; a violation here means a code path
        // constructed a Config without `validate()` — fail loudly, because
        // `.ok()` would silently disable the reporting the operator opted
        // into.
        dsn: Some(
            config
                .dsn
                .parse()
                .expect("sentry.dsn validated at config load"),
        ),
        release: sentry::release_name!(),
        environment: config.environment.clone().map(Into::into),
        // Usage/performance metrics are a separate opt-in from error
        // reporting; with this off, `crate::metrics` capture calls are dropped
        // by the client.
        enable_metrics: config.metrics,
        // The host name identifies the operator's machine; withhold it.
        before_send: Some(std::sync::Arc::new(|mut event| {
            event.server_name = None;
            Some(event)
        })),
        // Log fields can quote request-derived data (e.g. upstream error
        // bodies at warn level); keep only the breadcrumb message and level so
        // no log field ever leaves the machine — regardless of what existing
        // or future call sites put in their fields.
        before_breadcrumb: Some(std::sync::Arc::new(|mut breadcrumb| {
            breadcrumb.data.clear();
            Some(breadcrumb)
        })),
        ..Default::default()
    });
    tracing::info!(metrics = config.metrics, "sentry error reporting enabled");
    Some(guard)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("shunt=info"));
    tracing_subscriber::registry()
        .with(filter)
        // Logs go to stderr so command stdout (e.g. the `token` subcommand's
        // apiKeyHelper output) stays free of log noise.
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        // Forwards error! events to Sentry as events and warn!/info! as
        // breadcrumbs — a no-op unless `init_sentry` bound a client. Spans are
        // rejected entirely: shunt doesn't use Sentry tracing, and span fields
        // carry request-derived data (path, client session id) that would
        // otherwise ride into error events via the trace context.
        .with(sentry::integrations::tracing::layer().span_filter(|_| false))
        .init();
}
