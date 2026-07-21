use std::io::{ErrorKind, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;
use clap::{Parser, Subcommand};
use shunt::{
    blueprints::{self, AddKind},
    config::{Config, OtelConfig, SentryConfig},
    server,
    telemetry::{self, OtelReloadLayer, TelemetryGuard},
};
use tracing_subscriber::{
    layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter, Registry,
};

/// Handle to the subscriber's reloadable OTel layer slot, set once by
/// [`init_tracing`]. Stored globally so [`run`] can inject the OTel bridges
/// after config load without threading it through unrelated call sites.
type OtelReloadHandle = reload::Handle<OtelReloadLayer, Registry>;
static OTEL_RELOAD: OnceLock<OtelReloadHandle> = OnceLock::new();

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LoginMode {
    /// Copy the local `claude` login (~/.claude/.credentials.json). Refreshable.
    Import,
    /// Store a one-year, inference-only, non-refreshable setup token.
    SetupToken,
    /// Run shunt's own PKCE OAuth login and store a refreshable token.
    Oauth,
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
    /// Retrieve an embedded implementation blueprint for a coding agent.
    Add {
        /// Blueprint category (`upstream` or `provider`).
        #[arg(value_enum)]
        kind: Option<AddKind>,
        /// Known blueprint slug or an absolute http(s) research URL.
        #[arg(requires = "kind")]
        name_or_url: Option<String>,
        /// Print raw Markdown for piping to an agent.
        #[arg(long, requires = "name_or_url")]
        print: bool,
    },
    /// Log in to a subscription provider and save its credential for shunt to
    /// inject. Supports `xai`, `cursor`, `claude`, and `codex`.
    Login {
        /// Provider to log in to (`xai`, `cursor`, `claude`, or `codex`).
        provider: String,
        /// Stable account name used by a name-only pool entry (`claude` and
        /// `codex` only).
        #[arg(long)]
        name: Option<String>,
        /// Generate and store a one-year `claude setup-token` value (`claude`
        /// only; Codex OAuth tokens are always refreshable, so this does not
        /// apply to `shunt login codex`).
        #[arg(long)]
        long_lived: bool,
        /// Login method for `claude` (import | setup-token | oauth). Without it,
        /// prompts interactively on a TTY, else defaults to `import`. `--long-lived`
        /// is a deprecated alias for `--mode setup-token`.
        #[arg(long, value_enum, conflicts_with = "long_lived")]
        mode: Option<LoginMode>,
        /// For `--mode oauth`: skip the browser callback and paste the code manually.
        #[arg(long)]
        manual: bool,
    },
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Run { config }) => run(config.or(cli.config)),
        Some(Command::Check { config }) => check(config.or(cli.config)),
        Some(Command::Token) => runtime()?.block_on(token()),
        Some(Command::Add {
            kind,
            name_or_url,
            print,
        }) => add(kind, name_or_url.as_deref(), print),
        Some(Command::Login {
            provider,
            name,
            long_lived,
            mode,
            manual,
        }) => login(
            &provider,
            name.as_deref(),
            long_lived,
            mode,
            manual,
            cli.config.as_deref(),
        ),
        None if cli.check => check(cli.config),
        None => run(cli.config),
    }
}

fn add(kind: Option<AddKind>, name_or_url: Option<&str>, print: bool) -> anyhow::Result<()> {
    let output = match (kind, name_or_url) {
        (None, None) => blueprints::list(),
        (Some(kind), None) => blueprints::list_kind(kind),
        (Some(kind), Some(name_or_url)) => blueprints::resolve(kind, name_or_url)?,
        (None, Some(_)) => unreachable!("clap requires kind before name_or_url"),
    };
    let stdout = std::io::stdout();
    write_add_output(stdout.lock(), output.as_bytes())?;

    if let (Some(kind), Some(_)) = (kind, name_or_url) {
        if let Some(hint) = add_hint(kind, print, std::io::stderr().is_terminal()) {
            eprintln!("{hint}");
        }
    }
    Ok(())
}

fn add_hint(kind: AddKind, print: bool, is_tty: bool) -> Option<&'static str> {
    if print || !is_tty {
        return None;
    }

    Some(match kind {
        AddKind::Upstream => {
            "Hint: pipe this blueprint to an agent, for example: `shunt add upstream kimi --print | claude`"
        }
        AddKind::Provider => {
            "Hint: pipe this blueprint to an agent, for example: `shunt add provider https://example.com/docs --print | claude`"
        }
    })
}

fn write_add_output(mut writer: impl Write, output: &[u8]) -> std::io::Result<()> {
    let result = writer.write_all(output).and_then(|()| writer.flush());
    match result {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

/// `--manual` only affects the Claude OAuth browser-callback flow (it skips the
/// loopback callback in favour of a pasted code). It requires an explicit
/// `--mode oauth`: the interactive default and the non-interactive fallback both
/// resolve to `import`, which ignores the flag, so accepting `--manual` there
/// would silently run a different login than requested. Reject it everywhere
/// else so a mistyped invocation fails loudly.
fn ensure_manual_flag_valid(
    provider: &str,
    mode: Option<LoginMode>,
    manual: bool,
) -> anyhow::Result<()> {
    if !manual {
        return Ok(());
    }
    if provider != "claude" || mode != Some(LoginMode::Oauth) {
        anyhow::bail!("--manual is only valid with `shunt login claude --mode oauth`");
    }
    Ok(())
}

fn login(
    provider: &str,
    name: Option<&str>,
    long_lived: bool,
    mode: Option<LoginMode>,
    manual: bool,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    ensure_manual_flag_valid(provider, mode, manual)?;
    match provider {
        "xai" if name.is_none() && !long_lived && mode.is_none() => {
            runtime()?.block_on(shunt::auth::xai::login::run(provider))
        }
        "xai" => {
            anyhow::bail!(
                "--name, --long-lived, and --mode are only valid for `shunt login claude`"
            )
        }
        "cursor" if name.is_none() && !long_lived && mode.is_none() => runtime()?.block_on(async {
            // Logging in should not require a fully valid gateway config:
            // read the optional override best-effort and fall back to the
            // default Cursor host if the config fails to load or omits it.
            let default_base = Config::load(config_path)
                .ok()
                .and_then(|config| {
                    config
                        .provider("cursor")
                        .map(|provider| provider.base_url.clone())
                })
                .unwrap_or_else(|| "https://api2.cursor.sh".to_string());
            let base_url = shunt::auth::cursor::resolve_base_url(default_base);
            shunt::auth::cursor::login::run_with_base(&base_url).await
        }),
        "cursor" => {
            anyhow::bail!(
                "--name, --long-lived, and --mode are only valid for `shunt login claude`"
            )
        }
        "claude" => {
            let name = name.ok_or_else(|| {
                anyhow::anyhow!("`shunt login claude` requires --name <account-name>")
            })?;
            match resolve_claude_mode(mode, long_lived)? {
                LoginMode::Oauth => {
                    runtime()?.block_on(shunt::auth::claude::login::run_oauth(name, manual))
                }
                LoginMode::SetupToken => {
                    runtime()?.block_on(shunt::auth::claude::login::run(name, true))
                }
                LoginMode::Import => {
                    runtime()?.block_on(shunt::auth::claude::login::run(name, false))
                }
            }
        }
        "codex" if long_lived => {
            anyhow::bail!(
                "--long-lived is not supported for `shunt login codex`; Codex OAuth tokens are always refreshable"
            )
        }
        "codex" if mode.is_some() => {
            anyhow::bail!(
                "--mode is not supported for `shunt login codex`; Codex OAuth tokens are always refreshable"
            )
        }
        "codex" => {
            let name = name.ok_or_else(|| {
                anyhow::anyhow!("`shunt login codex` requires --name <account-name>")
            })?;
            runtime()?.block_on(shunt::auth::codex::login::run(name))
        }
        _ => {
            anyhow::bail!(
                "unknown login provider {provider:?}; supported: claude, codex, cursor, xai"
            )
        }
    }
}

/// Resolve the effective claude login mode: explicit `--mode` wins; `--long-lived`
/// maps to setup-token; otherwise prompt on a TTY, else default to import.
fn resolve_claude_mode(mode: Option<LoginMode>, long_lived: bool) -> anyhow::Result<LoginMode> {
    if let Some(mode) = mode {
        return Ok(mode);
    }
    if long_lived {
        return Ok(LoginMode::SetupToken);
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return prompt_claude_mode();
    }
    Ok(LoginMode::Import)
}

fn prompt_claude_mode() -> anyhow::Result<LoginMode> {
    use std::io::Write;
    println!("Select a Claude login method:");
    println!(
        "  1) oauth       — shunt runs the OAuth login, stores a refreshable token (recommended)"
    );
    println!("  2) import      — copy the local `claude` login (~/.claude/.credentials.json)");
    println!("  3) setup-token — one-year, inference-only, non-refreshable token");
    print!("Enter 1, 2, or 3 [1]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    match line.trim() {
        "" | "1" | "oauth" => Ok(LoginMode::Oauth),
        "2" | "import" => Ok(LoginMode::Import),
        "3" | "setup-token" => Ok(LoginMode::SetupToken),
        other => anyhow::bail!("invalid selection {other:?}; expected 1, 2, or 3"),
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
    let path = shunt::auth::claude::auth::default_credentials_path();
    let client = reqwest::Client::new();
    // stdout carries only the token so it can be consumed by apiKeyHelper.
    let token = shunt::auth::claude::auth::resolve_token(path, client).await?;
    println!("{token}");
    Ok(())
}

fn run(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    // Resolve the effective config path once at startup so reload/file-watch
    // reuse the exact same file the initial load used.
    let path = config_path.or_else(Config::find_config_file);
    let config = Config::load(path.as_deref()).context("failed to load config")?;
    // Both guards must outlive the runtime so buffered events flush on shutdown.
    let _sentry = init_sentry(config.sentry.as_ref());
    let _telemetry = init_telemetry(config.otel.as_ref());
    let result = runtime().and_then(|runtime| runtime.block_on(serve(config, path)));
    if let Err(error) = &result {
        sentry::integrations::anyhow::capture_anyhow(error);
    }
    result
}

async fn serve(config: Config, path: Option<PathBuf>) -> anyhow::Result<()> {
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
    let (router, shared, state) =
        server::build_router(config).context("failed to initialize gateway")?;
    // Reload triggers (SIGHUP and config-file watch) run as background tasks and
    // hot-swap the shared runtime state that the router reads per request.
    shunt::reload::spawn_reload_watchers(shared, path).await;
    // Opt-in `[server.pool] state_path`: warm-start the pool from the last
    // persisted quota before serving, then flush changes in the background. Both
    // are no-ops when the key is unset.
    shunt::state_persist::restore(&state).await;
    shunt::state_persist::spawn_state_persister(state.clone());
    // Opt-in `[server.gateway] state_path`: restore gateway-login refresh
    // sessions before serving; later mutations are written by the token
    // endpoint itself. A no-op when the key is unset.
    shunt::gateway::persist::restore(&state).await;
    // Opt-in `[server.pool] usage_refresh_seconds`: poll the Anthropic OAuth
    // usage API in the background, sharing the router's account pool. A no-op
    // when the key is unset.
    shunt::usage_poll::spawn_usage_poller(state);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
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
/// diagnostics only — fatal startup/serve errors, panics, and `error!` events,
/// never request/response bodies, headers, or credentials. Performance tracing
/// is a further opt-in via `[sentry] traces_sample_rate`; the span filter
/// installed by [`init_tracing`] admits spans only after this pins an enabled
/// policy.
fn init_sentry(config: Option<&SentryConfig>) -> Option<sentry::ClientInitGuard> {
    let config = config.filter(|sentry| sentry.enabled())?;
    let traces = config.traces_sample_rate > 0.0;
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
        attach_stacktrace: true,
        in_app_include: vec!["shunt"],
        // Usage/performance metrics are a separate opt-in from error
        // reporting; with this off, `crate::metrics` capture calls are dropped
        // by the client.
        enable_metrics: config.metrics,
        // Tracing is another separate opt-in: the rate (validated to
        // [0.0, 1.0] at config load) head-samples the transactions the span
        // filter lets through; at the 0.0 default the filter never admits a
        // span in the first place.
        traces_sample_rate: config.traces_sample_rate as f32,
        before_send: Some(std::sync::Arc::new(scrub_event)),
        // Log fields can quote request-derived data (e.g. upstream error
        // bodies at warn level); keep only the breadcrumb message and level so
        // no log field ever leaves the machine — regardless of what existing
        // or future call sites put in their fields.
        before_breadcrumb: Some(std::sync::Arc::new(|mut breadcrumb| {
            breadcrumb.data.clear();
            Some(breadcrumb)
        })),
        // Performance transactions (unlike error events) go straight from the
        // SDK to `send_envelope` and never pass through `before_send` — sentry
        // 0.48.4 has no `before_send_transaction` — so `scrub_event` cannot
        // strip the hostname from them. The `contexts` feature's
        // `ContextIntegration::setup` only auto-fills `server_name` with the
        // machine hostname `if options.server_name.is_none()`, so pin it to
        // empty here to preempt that at the source for both event kinds.
        server_name: Some("".into()),
        ..Default::default()
    });
    // Pin whether the subscriber's Sentry layer forwards spans — and whether
    // the request span may carry the client session id — for the process
    // lifetime; the Sentry client is built once and never rebuilt on reload.
    telemetry::pin_sentry_span_export(traces, config.include_session_id);
    tracing::info!(
        metrics = config.metrics,
        traces,
        "sentry error reporting enabled"
    );
    Some(guard)
}

/// The host name identifies the operator's machine; withhold it. This covers
/// error events (the only kind that reaches `before_send`); the transaction
/// path is instead handled by pinning `ClientOptions.server_name` to empty in
/// `init_sentry`, since transactions never pass through `before_send`.
fn scrub_event(
    mut event: sentry::protocol::Event<'static>,
) -> Option<sentry::protocol::Event<'static>> {
    event.server_name = None;
    Some(event)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("shunt=info"));
    // Empty OTel slot, swapped for the trace+logs bridges by `init_telemetry`
    // once config is loaded (the exporters need the endpoint). Placing the
    // reload layer first pins its subscriber type to `Registry`, so the layer
    // swapped in is a plain `Box<dyn Layer<Registry>>`. The global `filter`
    // still gates it — a disabled event is dropped for every layer, OTel
    // included — so exports stay scoped to `shunt` targets like the stderr logs.
    let none: OtelReloadLayer = None;
    let (otel_layer, otel_handle) = reload::Layer::new(none);
    tracing_subscriber::registry()
        .with(otel_layer)
        .with(filter)
        // Logs go to stderr so command stdout (e.g. the `token` subcommand's
        // apiKeyHelper output) stays free of log noise.
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        // Forwards error! events to Sentry as events and warn!/info! as
        // breadcrumbs — a no-op unless `init_sentry` bound a client. Spans
        // pass only when the operator opted into Sentry tracing via `[sentry]
        // traces_sample_rate`: the filter reads the decision `init_sentry`
        // pins after this subscriber is installed. Until then — and for
        // configs and commands that never enable tracing — every span is
        // rejected, because span fields carry request-derived data (path,
        // client session id) that would otherwise ride into error events via
        // the trace context.
        .with(
            sentry::integrations::tracing::layer()
                .span_filter(|_| telemetry::sentry_span_export_enabled()),
        )
        .init();
    // Only the first init wins (later calls in tests are ignored); a failure to
    // store the handle just leaves OTel disabled, never a crash.
    let _ = OTEL_RELOAD.set(otel_handle);
}

/// Opt-in OpenTelemetry export: build the OTLP pipeline only when the operator
/// configured a non-empty `[otel] endpoint`, then swap the trace+logs bridges
/// into the subscriber's reload slot. Export failures are non-fatal — shunt
/// keeps serving without telemetry rather than refusing to boot.
fn init_telemetry(config: Option<&OtelConfig>) -> Option<TelemetryGuard> {
    let config = config.filter(|otel| otel.enabled())?;
    match telemetry::init(config) {
        Ok((guard, layer)) => {
            match OTEL_RELOAD.get() {
                Some(handle) => {
                    if let Err(error) = handle.reload(layer) {
                        tracing::warn!(%error, "failed to install otel trace/logs layer; metrics still export");
                    }
                }
                // Unreachable in the shipped binary (init_tracing runs first),
                // but warn loudly rather than silently drop trace/logs export if
                // a future reordering ever leaves the slot unset.
                None => tracing::warn!(
                    "otel reload slot unset (init_tracing did not run); trace/logs export disabled, metrics still export"
                ),
            }
            tracing::info!(
                endpoint = %config.endpoint,
                traces = config.traces,
                metrics = config.metrics,
                logs = config.logs,
                "opentelemetry export enabled"
            );
            Some(guard)
        }
        Err(error) => {
            tracing::error!(%error, "failed to initialize opentelemetry export; continuing without it");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    struct FailingWriter(ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn add_hint_is_emitted_for_each_kind_on_a_tty_without_print() {
        let upstream = add_hint(AddKind::Upstream, false, true).unwrap();
        assert!(upstream.contains("shunt add upstream kimi --print | claude"));

        let provider = add_hint(AddKind::Provider, false, true).unwrap();
        assert!(provider.contains("shunt add provider https://example.com/docs --print | claude"));
    }

    #[test]
    fn add_hint_is_suppressed_for_print_or_non_tty_stderr() {
        for kind in [AddKind::Upstream, AddKind::Provider] {
            assert!(add_hint(kind, true, true).is_none());
            assert!(add_hint(kind, false, false).is_none());
            assert!(add_hint(kind, true, false).is_none());
        }
    }

    #[test]
    fn add_output_treats_broken_pipe_as_success() {
        assert!(write_add_output(FailingWriter(ErrorKind::BrokenPipe), b"blueprint").is_ok());
    }

    #[test]
    fn add_output_propagates_other_write_errors() {
        let error =
            write_add_output(FailingWriter(ErrorKind::WriteZero), b"blueprint").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::WriteZero);
    }

    #[test]
    fn add_command_parses_listing_forms() {
        let parsed = Cli::try_parse_from(["shunt", "add"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Add {
                kind: None,
                name_or_url: None,
                print: false
            })
        ));

        let parsed = Cli::try_parse_from(["shunt", "add", "upstream"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Add {
                kind: Some(AddKind::Upstream),
                name_or_url: None,
                print: false
            })
        ));
    }

    #[test]
    fn add_command_parses_retrieval_with_print() {
        let parsed = Cli::try_parse_from(["shunt", "add", "upstream", "kimi", "--print"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Add {
                kind: Some(AddKind::Upstream),
                name_or_url: Some(ref value),
                print: true
            }) if value == "kimi"
        ));
    }

    #[test]
    fn add_command_rejects_name_without_kind() {
        assert!(Cli::try_parse_from(["shunt", "add", "kimi"]).is_err());
        assert!(Cli::try_parse_from(["shunt", "add", "--print"]).is_err());
    }

    #[test]
    fn manual_flag_without_flag_is_always_valid() {
        assert!(ensure_manual_flag_valid("xai", None, false).is_ok());
        assert!(ensure_manual_flag_valid("claude", Some(LoginMode::Import), false).is_ok());
    }

    #[test]
    fn manual_flag_allows_only_explicit_claude_oauth() {
        assert!(ensure_manual_flag_valid("claude", Some(LoginMode::Oauth), true).is_ok());
    }

    #[test]
    fn manual_flag_rejected_when_mode_is_not_explicit_oauth() {
        // The interactive default and the non-interactive fallback both resolve to
        // `import`, which ignores --manual, so mode: None must be rejected too
        // (only an explicit --mode oauth accepts the flag).
        assert!(ensure_manual_flag_valid("claude", None, true).is_err());
        assert!(ensure_manual_flag_valid("claude", Some(LoginMode::Import), true).is_err());
        assert!(ensure_manual_flag_valid("claude", Some(LoginMode::SetupToken), true).is_err());
    }

    #[test]
    fn manual_flag_rejected_for_non_claude_providers() {
        assert!(ensure_manual_flag_valid("xai", Some(LoginMode::Oauth), true).is_err());
        assert!(ensure_manual_flag_valid("cursor", None, true).is_err());
        assert!(ensure_manual_flag_valid("codex", None, true).is_err());
    }

    #[test]
    fn claude_login_requires_name_and_accepts_long_lived() {
        assert!(Cli::try_parse_from(["shunt", "login", "claude", "--name", "ci"]).is_ok());
        assert!(
            Cli::try_parse_from(["shunt", "login", "claude", "--name", "ci", "--long-lived"])
                .is_ok()
        );
        let parsed = Cli::try_parse_from(["shunt", "login", "claude"]).unwrap();
        let Some(Command::Login {
            provider,
            name,
            long_lived,
            mode,
            manual,
        }) = parsed.command
        else {
            panic!("expected login command");
        };
        assert_eq!(provider, "claude");
        assert!(name.is_none());
        assert!(!long_lived);
        assert!(mode.is_none());
        assert!(!manual);
    }

    #[test]
    fn claude_login_modes_parse_and_conflict_with_long_lived() {
        for (value, expected) in [
            ("oauth", LoginMode::Oauth),
            ("import", LoginMode::Import),
            ("setup-token", LoginMode::SetupToken),
        ] {
            let parsed =
                Cli::try_parse_from(["shunt", "login", "claude", "--name", "ci", "--mode", value])
                    .unwrap();
            let Some(Command::Login { mode, .. }) = parsed.command else {
                panic!("expected login command");
            };
            assert_eq!(mode, Some(expected));
        }
        assert!(Cli::try_parse_from([
            "shunt",
            "login",
            "claude",
            "--name",
            "ci",
            "--mode",
            "oauth",
            "--long-lived",
        ])
        .is_err());
    }

    #[test]
    fn resolves_explicit_and_long_lived_claude_modes() {
        assert_eq!(
            resolve_claude_mode(Some(LoginMode::Oauth), false).unwrap(),
            LoginMode::Oauth
        );
        assert_eq!(
            resolve_claude_mode(None, true).unwrap(),
            LoginMode::SetupToken
        );
    }

    #[test]
    fn codex_login_parses_name_and_rejects_missing_name_or_long_lived() {
        assert!(Cli::try_parse_from(["shunt", "login", "codex", "--name", "ci"]).is_ok());
        let parsed = Cli::try_parse_from(["shunt", "login", "codex", "--name", "ci"]).unwrap();
        let Some(Command::Login {
            provider,
            name,
            long_lived,
            mode,
            manual,
        }) = parsed.command
        else {
            panic!("expected login command");
        };
        assert_eq!(provider, "codex");
        assert_eq!(name.as_deref(), Some("ci"));
        assert!(!long_lived);
        assert!(mode.is_none());
        assert!(!manual);

        // These error branches return before touching the network or runtime,
        // so they are safe to exercise directly (mirrors the pattern used for
        // the other providers' bail arms below).
        let error =
            login("codex", None, false, None, false, None).expect_err("missing --name must fail");
        assert!(error.to_string().contains("requires --name"));

        let error = login("codex", Some("ci"), true, None, false, None)
            .expect_err("--long-lived must be rejected for codex");
        assert!(error.to_string().contains("--long-lived is not supported"));

        let error = login(
            "codex",
            Some("ci"),
            false,
            Some(LoginMode::Oauth),
            false,
            None,
        )
        .expect_err("--mode must be rejected for codex");
        assert!(error.to_string().contains("--mode is not supported"));
    }

    #[test]
    fn login_rejects_unknown_provider() {
        let error = login("unknown", None, false, None, false, None)
            .expect_err("unknown provider must fail");
        assert!(error.to_string().contains("unknown login provider"));
    }

    #[test]
    fn runtime_builds() {
        assert!(runtime().is_ok());
    }

    #[test]
    fn init_sentry_without_config_creates_no_client() {
        assert!(init_sentry(None).is_none());
    }

    #[test]
    fn init_sentry_with_blank_dsn_creates_no_client() {
        let config = SentryConfig {
            dsn: "   ".to_string(),
            environment: None,
            metrics: false,
            traces_sample_rate: 0.0,
            include_session_id: false,
        };
        assert!(init_sentry(Some(&config)).is_none());
    }

    #[test]
    fn init_sentry_with_valid_dsn_binds_client() {
        let config = SentryConfig {
            dsn: "https://public@sentry.invalid/1".to_string(),
            environment: Some("test".to_string()),
            metrics: false,
            traces_sample_rate: 0.0,
            include_session_id: false,
        };
        let guard = init_sentry(Some(&config));
        let guard = guard.expect("valid dsn binds a client");
        // Tracing stayed at its 0.0 default, so the pinned policy keeps the
        // subscriber's Sentry span filter closed — the pre-tracing behavior.
        assert!(!telemetry::sentry_span_export_enabled());
        // The empty server_name pin must survive client init: transactions
        // bypass before_send/scrub_event, so this field is the only thing
        // standing between a traced request and the machine hostname (the
        // contexts integration auto-fills it only when left None).
        assert_eq!(guard.options().server_name, Some("".into()));
    }

    #[test]
    fn scrub_event_withholds_server_name() {
        let event = sentry::protocol::Event {
            server_name: Some("operator-laptop".into()),
            ..Default::default()
        };
        let scrubbed = scrub_event(event).expect("scrubbing keeps the event");
        assert!(scrubbed.server_name.is_none());
    }

    #[test]
    fn serve_rejects_invalid_bind_address() {
        let mut config = Config::default();
        config.server.bind = "not-an-address".to_string();
        let error = runtime()
            .expect("runtime builds")
            .block_on(serve(config, None))
            .expect_err("invalid bind must fail");
        assert!(error.to_string().contains("invalid server bind address"));
    }

    #[test]
    fn run_surfaces_serve_errors() {
        // Hold a loopback port so `serve` deterministically fails to bind it.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("reserve test bind address");
        let bind = listener.local_addr().expect("read reserved bind address");
        // Unique directory so concurrent `cargo test` invocations on the same
        // machine can't collide on the config file.
        let dir = std::env::temp_dir().join(format!(
            "shunt-run-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        // RAII guard so the directory is removed even when an assertion
        // below panics.
        struct TempDirGuard(std::path::PathBuf);
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = TempDirGuard(dir.clone());

        let path = dir.join("shunt.toml");
        std::fs::write(&path, format!("[server]\nbind = \"{bind}\"\n")).expect("write test config");
        let result = run(Some(path.clone()));
        drop(listener);
        assert!(result
            .expect_err("occupied address must fail")
            .to_string()
            .contains("failed to bind"));
    }
}
