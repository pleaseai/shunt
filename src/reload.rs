//! Hot configuration reload for a long-running shared gateway.
//!
//! The live config is held behind an [`arc_swap::ArcSwap`] so a reload swaps in
//! a new [`RuntimeState`] atomically without locking readers. Two triggers call
//! [`reload`]: a `SIGHUP` signal (`kill -HUP <pid>`) and automatic detection of
//! config-file changes (a `notify` watcher on the file's parent directory).
//!
//! Reload is fail-safe: [`reload`] loads and validates the new config in full
//! before swapping, and on any error it returns without touching the live state,
//! so an invalid edit never takes the process down or leaves it running open.
//! Each request snapshots the live state on entry (see `AppState::refreshed`),
//! so an in-flight request never sees config change underneath it.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::{
    admin::AdminAuth,
    auth::inbound::InboundAuth,
    config::{Config, ConfigError, SentryConfig},
};

/// The hot-swappable runtime state derived from a loaded config: the config
/// itself plus anything resolved from it that a request reads on every call.
pub struct RuntimeState {
    pub config: Arc<Config>,
    /// Inbound client-token auth (`[server.auth]`), re-resolved on every reload
    /// so token/header edits take effect. `None` ⇒ open (no inbound auth).
    pub inbound_auth: Option<Arc<InboundAuth>>,
    /// Admin-surface auth (`[server.admin]`), re-resolved on every reload so
    /// admin token/header edits take effect. `None` ⇒ admin surface disabled
    /// (its routes, when registered at boot, then reject every request).
    pub admin_auth: Option<Arc<AdminAuth>>,
}

/// Shared handle to the live [`RuntimeState`]. Cloning is cheap (an `Arc`); a
/// reload replaces the pointed-to state with [`ArcSwap::store`].
pub type SharedState = Arc<ArcSwap<RuntimeState>>;

impl RuntimeState {
    /// Build runtime state from an already-loaded config. `Config::load`
    /// validates, but a config constructed by other means might not have, so
    /// validate defensively before resolving auth.
    pub fn from_config(config: Config) -> Result<Self, ConfigError> {
        let config = config.validate()?;
        let inbound_auth = config.resolve_inbound_auth()?;
        let admin_auth = config.resolve_admin_auth()?;
        Ok(Self {
            config: Arc::new(config),
            inbound_auth,
            admin_auth,
        })
    }
}

/// Reload the config from `path` and, only on full success, atomically swap it
/// into `shared`. On any error the currently-live config stays untouched and the
/// error is returned for the caller to log — the gateway keeps running the last
/// good config rather than going down or running open.
///
/// Fields that cannot be hot-applied (`server.bind`, `[sentry]`) are compared
/// against the live config and a `warn!` is logged when they change; the new
/// values are accepted into the swapped config but only take effect on restart.
pub fn reload(shared: &SharedState, path: Option<&std::path::Path>) -> Result<(), ConfigError> {
    // Load + validate the candidate before touching the live state.
    let new_config = Config::load(path)?;
    let previous = shared.load();
    warn_on_restart_only_changes(&previous.config, &new_config);
    let new_state = RuntimeState::from_config(new_config)?;
    shared.store(Arc::new(new_state));
    tracing::info!("configuration reloaded successfully");
    Ok(())
}

/// Warn about fields that a hot reload cannot apply, so an operator relying on
/// the change is not misled into thinking it took effect.
fn warn_on_restart_only_changes(previous: &Config, next: &Config) {
    if previous.server.bind != next.server.bind {
        tracing::warn!(
            previous = %previous.server.bind,
            next = %next.server.bind,
            "server.bind changed but requires a restart to apply; the listener is already bound"
        );
    }
    // Whether the admin route tree is registered is decided once at boot from
    // the initial config (like `server.bind`). Token/header edits within an
    // already-enabled `[server.admin]` hot-apply via `admin_auth`, but toggling
    // the block on or off cannot add or remove the routes without a restart.
    if previous.server.admin.is_some() != next.server.admin.is_some() {
        tracing::warn!(
            "[server.admin] was enabled or disabled but requires a restart to register or drop its routes; \
             on a still-registered surface, disabling it makes every admin route reject requests"
        );
    }
    // Like `[server.admin]`, whether the inbound Responses routes are registered
    // is decided once at boot from the initial config. A hot edit that only
    // changes the target `provider` does take effect (it is read per request from
    // the swapped config), but toggling the block on or off cannot add or drop
    // the routes without a restart.
    if previous.server.codex_endpoint.is_some() != next.server.codex_endpoint.is_some() {
        tracing::warn!(
            "[server.codex_endpoint] was enabled or disabled but requires a restart to register or drop its routes"
        );
    }
    // The client-facing usage route is also fixed at boot. Token edits hot-apply,
    // but toggling the table cannot register or drop `/usage` without a restart.
    if previous.server.usage.is_some() != next.server.usage.is_some() {
        tracing::warn!(
            "[server.usage] was enabled or disabled but requires a restart to register or drop its route"
        );
    }
    if sentry_changed(previous.sentry.as_ref(), next.sentry.as_ref()) {
        tracing::warn!(
            "[sentry] configuration changed but requires a restart to apply; the Sentry client is initialized once at startup"
        );
    }
    // `[otel]` is initialized once at startup (`init_telemetry`, before the
    // hot-reload state exists) and its OTLP providers are never reconstructed on
    // reload, so — like `[sentry]` — warn rather than silently accept an edit
    // that won't take effect. `OtelConfig` derives `PartialEq`, so compare the
    // optional sections directly.
    if previous.otel != next.otel {
        tracing::warn!(
            "[otel] configuration changed but requires a restart to apply; the OpenTelemetry exporters are initialized once at startup"
        );
    }
}

/// Structural comparison of two optional `[sentry]` sections. `SentryConfig`
/// does not derive `PartialEq`, so compare the fields that matter.
fn sentry_changed(previous: Option<&SentryConfig>, next: Option<&SentryConfig>) -> bool {
    match (previous, next) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            a.dsn != b.dsn
                || a.environment != b.environment
                || a.metrics != b.metrics
                || a.traces_sample_rate != b.traces_sample_rate
                || a.include_session_id != b.include_session_id
        }
        _ => true,
    }
}

/// Debounce window: filesystem writes arrive in bursts (editors write, rename,
/// chmod; Kubernetes swaps a ConfigMap symlink), so a relevant event starts a
/// quiet timer that later events restart, and the reload fires once the writes
/// settle. Long enough to coalesce a burst, short enough to feel immediate.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// Spawn the reload triggers as background tasks and return. On Unix a `SIGHUP`
/// handler reloads on each signal. When `path` is set, a `notify` watcher on the
/// config file's parent directory reloads (debounced) on file changes. Watcher
/// setup failures are logged and skipped — the gateway keeps running (and SIGHUP
/// still works) rather than aborting.
pub async fn spawn_reload_watchers(shared: SharedState, path: Option<std::path::PathBuf>) {
    #[cfg(unix)]
    spawn_sighup_task(shared.clone(), path.clone());

    if let Some(path) = path {
        spawn_file_watch_task(shared, path);
    }
}

/// Run a [`reload`] on the blocking thread pool so its synchronous file I/O
/// (`Config::load` reads and parses the file) never stalls an async worker: a
/// slow or network-mounted config could otherwise pause request handling on the
/// shared runtime. Logs `failure_context` on a reload error, and separately if
/// the blocking task itself panics.
async fn reload_off_thread(
    shared: &SharedState,
    path: Option<&std::path::Path>,
    failure_context: &'static str,
) {
    let shared = shared.clone();
    let path = path.map(std::path::Path::to_path_buf);
    match tokio::task::spawn_blocking(move || reload(&shared, path.as_deref())).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "{failure_context}"),
        Err(join_error) => {
            tracing::error!(%join_error, "reload task panicked; keeping the running configuration")
        }
    }
}

/// Reload on each `SIGHUP` (`kill -HUP <pid>`). Unix-only; on other platforms
/// SIGHUP does not exist and only the file watcher drives reloads.
#[cfg(unix)]
fn spawn_sighup_task(shared: SharedState, path: Option<std::path::PathBuf>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut signal = match signal(SignalKind::hangup()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::warn!(%error, "failed to install SIGHUP handler; reload-on-signal disabled");
            return;
        }
    };
    tokio::spawn(async move {
        while signal.recv().await.is_some() {
            tracing::info!("received SIGHUP, reloading configuration");
            reload_off_thread(
                &shared,
                path.as_deref(),
                "SIGHUP reload failed; keeping the running configuration",
            )
            .await;
        }
    });
}

/// Watch the config file's parent directory (not the file inode) so atomic-rename
/// saves and Kubernetes ConfigMap symlink swaps — which replace the file rather
/// than write in place — are still detected. Events are bridged from `notify`'s
/// std channel onto a tokio channel and debounced before each reload.
fn spawn_file_watch_task(shared: SharedState, path: std::path::PathBuf) {
    use notify::{RecursiveMode, Watcher};

    let watch_dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // notify calls the handler on its own thread with std types; forward only
    // events that touch the config file onto a tokio channel so the async task
    // can debounce them. Filtering here (rather than in the async task) means an
    // unrelated sibling write in the watched directory never reaches the debounce
    // timer and so cannot reset it — a continuously-active writer in the same
    // directory can no longer starve a real config change indefinitely.
    let watch_path = path.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = match notify::recommended_watcher(
        move |result: notify::Result<notify::Event>| match result {
            Ok(event) => {
                // Skip access (read/open) events: a reload reads the config file,
                // and that read fires an access event on the watched directory —
                // forwarding it would retrigger reload on our own read, a loop.
                // Only forward real changes that touch the config file.
                if !event.kind.is_access() && event_touches_path(&event, &watch_path) {
                    // A closed receiver just means the server is shutting down.
                    let _ = tx.send(event);
                }
            }
            // Surface watcher errors rather than silently degrading: the watch may
            // be impaired (e.g. inotify queue overflow) and the operator should know.
            Err(error) => {
                tracing::warn!(%error, "config file watcher error");
            }
        },
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(%error, "failed to create config file watcher; auto-reload on file change disabled (SIGHUP still works)");
            return;
        }
    };
    if let Err(error) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
        tracing::warn!(%error, dir = %watch_dir.display(), "failed to watch config directory; auto-reload on file change disabled (SIGHUP still works)");
        return;
    }
    tracing::info!(dir = %watch_dir.display(), "watching config directory for changes");

    tokio::spawn(async move {
        // Keep the watcher alive for the lifetime of this task; dropping it stops
        // event delivery.
        let _watcher = watcher;
        loop {
            // Block until a config-relevant event arrives. Sibling events are
            // filtered out in the watcher callback, so every event delivered here
            // touches the config file and may legitimately extend the debounce.
            if rx.recv().await.is_none() {
                break;
            }
            // Debounce: keep draining until the writes go quiet, coalescing a
            // burst of events into a single reload. Only config-relevant events
            // reach the channel, so a sibling write can never restart this timer.
            loop {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(Some(_)) => continue, // more events; keep waiting for quiet
                    Ok(None) => break,       // channel closed; reload once below
                    Err(_) => break,         // quiet period elapsed
                }
            }
            tracing::info!("detected config file change, reloading configuration");
            reload_off_thread(
                &shared,
                Some(path.as_path()),
                "config file reload failed; keeping the running configuration",
            )
            .await;
        }
    });
}

/// Whether a filesystem event concerns the config file. Directory watching
/// surfaces sibling files too, so filter by the config file's own path (matching
/// its final component covers atomic renames whose event carries the temp path
/// then the final path). A Kubernetes ConfigMap mount is a special case: the
/// config symlink itself never fires an event on update — kubelet atomically
/// swaps the `..data` symlink that the config path resolves through, so accept a
/// `..data` event too or a mounted ConfigMap change would never hot-reload.
fn event_touches_path(event: &notify::Event, path: &std::path::Path) -> bool {
    let name = path.file_name();
    let configmap_data = std::ffi::OsStr::new("..data");
    event.paths.iter().any(|event_path| {
        event_path == path
            || (name.is_some() && event_path.file_name() == name)
            || event_path.file_name() == Some(configmap_data)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;

    use super::{
        event_touches_path, reload, sentry_changed, spawn_reload_watchers, RuntimeState,
        SharedState,
    };
    use crate::config::{Config, SentryConfig};

    /// Unique temp dir per test so concurrent `cargo test` runs never collide.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shunt-reload-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    struct TempDirGuard(std::path::PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn shared_from(config: Config) -> SharedState {
        Arc::new(ArcSwap::from_pointee(
            RuntimeState::from_config(config).expect("valid initial config"),
        ))
    }

    /// Capture tracing output for a closure, so warn-path assertions can inspect
    /// the emitted logs (mirrors config.rs's log-capture test).
    fn capture_logs(run: impl FnOnce()) -> String {
        use std::io::{self, Write};
        use std::sync::Mutex;

        struct BufferWriter {
            buffer: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for BufferWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.buffer.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = output.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn reload_swaps_in_a_valid_new_config() {
        let dir = temp_dir("valid");
        let _guard = TempDirGuard(dir.clone());
        let path = dir.join("shunt.toml");

        // Start from a config whose default_provider is anthropic.
        std::fs::write(&path, "[server]\ndefault_provider = \"anthropic\"\n").unwrap();
        let shared = shared_from(Config::load(Some(&path)).unwrap());
        assert_eq!(shared.load().config.server.default_provider, "anthropic");

        // Rewrite the file and reload; the live state must reflect the change.
        std::fs::write(&path, "[server]\ndefault_provider = \"openai\"\n").unwrap();
        reload(&shared, Some(&path)).expect("valid reload succeeds");
        assert_eq!(shared.load().config.server.default_provider, "openai");
    }

    #[test]
    fn reload_with_invalid_config_keeps_previous_state() {
        let dir = temp_dir("invalid");
        let _guard = TempDirGuard(dir.clone());
        let path = dir.join("shunt.toml");

        std::fs::write(&path, "[server]\ndefault_provider = \"anthropic\"\n").unwrap();
        let shared = shared_from(Config::load(Some(&path)).unwrap());

        // default_provider referencing an unknown provider fails validation.
        std::fs::write(&path, "[server]\ndefault_provider = \"nonexistent\"\n").unwrap();
        let error = reload(&shared, Some(&path)).expect_err("invalid reload must fail");
        assert!(error.to_string().contains("unknown provider: nonexistent"));
        // Fail-safe: the previously-live config is untouched.
        assert_eq!(shared.load().config.server.default_provider, "anthropic");
    }

    #[test]
    fn reload_reresolves_inbound_auth() {
        let dir = temp_dir("auth");
        let _guard = TempDirGuard(dir.clone());
        let path = dir.join("shunt.toml");
        let env = format!("SHUNT_RELOAD_TEST_TOKENS_{}", std::process::id());

        // Start with no inbound auth.
        std::fs::write(&path, "[server]\ndefault_provider = \"anthropic\"\n").unwrap();
        let shared = shared_from(Config::load(Some(&path)).unwrap());
        assert!(shared.load().inbound_auth.is_none());

        // Add [server.auth] pointing at an env var holding a valid token.
        std::env::set_var(&env, "alice:tok-a");
        std::fs::write(
            &path,
            format!(
                "[server]\ndefault_provider = \"anthropic\"\n\n[server.auth]\ntokens_env = \"{env}\"\n"
            ),
        )
        .unwrap();
        reload(&shared, Some(&path)).expect("reload with auth succeeds");
        assert!(shared.load().inbound_auth.is_some());
        std::env::remove_var(&env);
    }

    #[test]
    fn bind_change_warns_and_reload_still_succeeds() {
        let dir = temp_dir("bind");
        let _guard = TempDirGuard(dir.clone());
        let path = dir.join("shunt.toml");

        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:3001\"\n").unwrap();
        let shared = shared_from(Config::load(Some(&path)).unwrap());

        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:4002\"\n").unwrap();
        let logs = capture_logs(|| {
            reload(&shared, Some(&path)).expect("reload succeeds despite bind change");
        });

        // The reload succeeded and the new bind is stored in the config...
        assert_eq!(shared.load().config.server.bind, "127.0.0.1:4002");
        // ...but the operator was warned it requires a restart to take effect.
        assert!(logs.contains("server.bind changed"));
        assert!(logs.contains("requires a restart"));
    }

    #[test]
    fn sentry_change_warns_but_reload_still_succeeds() {
        let dir = temp_dir("sentry");
        let _guard = TempDirGuard(dir.clone());
        let path = dir.join("shunt.toml");

        // Start with no [sentry] section.
        std::fs::write(&path, "[server]\ndefault_provider = \"anthropic\"\n").unwrap();
        let shared = shared_from(Config::load(Some(&path)).unwrap());

        // Add a (disabled, empty-DSN) [sentry] section: a change that only takes
        // effect on restart, so the reload succeeds but warns.
        std::fs::write(
            &path,
            "[server]\ndefault_provider = \"anthropic\"\n\n[sentry]\ndsn = \"\"\n",
        )
        .unwrap();
        let logs = capture_logs(|| {
            reload(&shared, Some(&path)).expect("reload succeeds despite sentry change");
        });

        assert!(shared.load().config.sentry.is_some());
        assert!(logs.contains("[sentry] configuration changed"));
        assert!(logs.contains("requires a restart"));
    }

    #[test]
    fn sentry_changed_compares_presence_and_fields() {
        let base = SentryConfig {
            dsn: "https://public@o0.ingest.sentry.io/1".to_string(),
            environment: Some("prod".to_string()),
            metrics: false,
            traces_sample_rate: 0.0,
            include_session_id: false,
        };
        // Same values ⇒ unchanged; presence changes and field changes ⇒ changed.
        assert!(!sentry_changed(None, None));
        assert!(!sentry_changed(Some(&base), Some(&base.clone())));
        assert!(sentry_changed(None, Some(&base)));
        assert!(sentry_changed(Some(&base), None));

        let other_dsn = SentryConfig {
            dsn: "https://public@o0.ingest.sentry.io/2".to_string(),
            ..base.clone()
        };
        assert!(sentry_changed(Some(&base), Some(&other_dsn)));
        let other_env = SentryConfig {
            environment: None,
            ..base.clone()
        };
        assert!(sentry_changed(Some(&base), Some(&other_env)));
        let other_metrics = SentryConfig {
            metrics: true,
            ..base.clone()
        };
        assert!(sentry_changed(Some(&base), Some(&other_metrics)));
        let other_rate = SentryConfig {
            traces_sample_rate: 0.5,
            ..base.clone()
        };
        assert!(sentry_changed(Some(&base), Some(&other_rate)));
        let other_session_id = SentryConfig {
            include_session_id: true,
            ..base.clone()
        };
        assert!(sentry_changed(Some(&base), Some(&other_session_id)));
    }

    #[test]
    fn event_touches_path_matches_file_and_ignores_siblings() {
        let dir = std::path::Path::new("/etc/shunt");
        let config = dir.join("shunt.toml");

        // Exact path match.
        let exact = notify::Event::new(notify::EventKind::Any).add_path(config.clone());
        assert!(event_touches_path(&exact, &config));

        // Same filename under a different directory (atomic-rename / ConfigMap
        // symlink swap surfaces the final component).
        let by_name =
            notify::Event::new(notify::EventKind::Any).add_path(dir.join("..data/shunt.toml"));
        assert!(event_touches_path(&by_name, &config));

        // Kubernetes ConfigMap update: kubelet atomically swaps the `..data`
        // symlink, and that rename — not the config symlink — is the only event
        // that fires, so a bare `..data` event must count as touching the config.
        let configmap_swap =
            notify::Event::new(notify::EventKind::Any).add_path(dir.join("..data"));
        assert!(event_touches_path(&configmap_swap, &config));

        // An unrelated sibling in the watched directory must be ignored.
        let sibling = notify::Event::new(notify::EventKind::Any).add_path(dir.join("other.toml"));
        assert!(!event_touches_path(&sibling, &config));
    }

    #[test]
    fn from_config_rejects_invalid_config() {
        // default_provider pointing at an unknown provider fails validation, so
        // building runtime state from it errors rather than swapping in bad state.
        let mut config = Config::default();
        config.server.default_provider = "nope".to_string();
        // `RuntimeState` is not `Debug`, so match rather than `expect_err`.
        let error = match RuntimeState::from_config(config) {
            Ok(_) => panic!("invalid config must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unknown provider: nope"));
    }

    #[tokio::test]
    async fn file_watch_task_hot_reloads_on_file_change() {
        let dir = temp_dir("watch");
        let _guard = TempDirGuard(dir.clone());
        let path = dir.join("shunt.toml");

        std::fs::write(&path, "[server]\ndefault_provider = \"anthropic\"\n").unwrap();
        let shared = shared_from(Config::load(Some(&path)).unwrap());
        assert_eq!(shared.load().config.server.default_provider, "anthropic");

        // Start the watchers (SIGHUP + file watch) against the real file, then
        // change the file and wait for the debounced watcher to hot-swap it.
        spawn_reload_watchers(shared.clone(), Some(path.clone())).await;

        std::fs::write(&path, "[server]\ndefault_provider = \"openai\"\n").unwrap();

        // Poll for the reload rather than sleeping a fixed time: filesystem
        // notifications and the debounce window make timing non-deterministic.
        let mut reloaded = false;
        for _ in 0..80 {
            if shared.load().config.server.default_provider == "openai" {
                reloaded = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            reloaded,
            "file watcher should have hot-reloaded the changed config"
        );
    }
}
