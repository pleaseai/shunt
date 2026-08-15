//! Antigravity subscription OAuth: credential store, login, and the client
//! version fingerprint the backend is addressed with.

use std::{env, path::PathBuf};

use crate::config::{Config, ProviderKind};

pub mod auth;
pub mod login;
pub mod version;

/// shunt-owned Antigravity credential file: `$SHUNT_ANTIGRAVITY_AUTH_FILE`, else
/// `~/.shunt/antigravity-auth.json`. Written by `shunt login antigravity` and
/// refreshed by shunt alone — unlike the Gemini path, no other tool owns it.
pub fn default_antigravity_auth_path() -> PathBuf {
    env::var_os("SHUNT_ANTIGRAVITY_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            // `HOME` is unset on Windows; fall back to `USERPROFILE` so the
            // credential lands in the user's home rather than a
            // working-directory-relative path.
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
                .map(PathBuf::from)
                .map(|home| home.join(".shunt").join("antigravity-auth.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".shunt/antigravity-auth.json"))
}

/// Whether `config` routes any request path to the given built-in-kind
/// provider: its `default_provider`, an exact `route`, a `route_prefix`, or a
/// per-model `upstream_model` map entry.
fn routes_to_kind(config: &Config, kind: ProviderKind) -> bool {
    let is_kind = |name: &str| {
        config
            .providers
            .get(name)
            .is_some_and(|provider| provider.kind == kind)
    };

    is_kind(&config.server.default_provider)
        || config.routes.iter().any(|route| is_kind(&route.provider))
        || config
            .route_prefixes
            .iter()
            .any(|prefix| is_kind(&prefix.provider))
        || config.models.iter().any(|model| {
            model
                .upstream_model
                .as_ref()
                .is_some_and(|map| map.keys().any(|provider| is_kind(provider)))
        })
}

/// Whether the deprecated `agy` subprocess transport is reachable.
pub fn routes_to_antigravity_cli(config: &Config) -> bool {
    routes_to_kind(config, ProviderKind::AntigravityCli)
}

/// Whether the native Antigravity HTTP upstream is reachable.
pub fn routes_to_antigravity(config: &Config) -> bool {
    routes_to_kind(config, ProviderKind::Antigravity)
}

/// Issue #368 Stage 1: warn when config still routes to the deprecated `agy`
/// subprocess transport. Split from the call site so the warning is testable
/// without spinning up the server.
pub fn warn_if_routes_to_antigravity_cli(config: &Config) {
    if routes_to_antigravity_cli(config) {
        tracing::warn!(
            "config routes to `kind = \"antigravity_cli\"`, which is deprecated. \
             Switch to `kind = \"antigravity\"` for the native HTTP transport."
        );
    }
}

/// The boot/reload refusal for a routed native Antigravity provider with no
/// credential. Split from the filesystem probe so the message is testable
/// without depending on what happens to exist in the test environment's home
/// directory. Shared by `main.rs`'s `serve()` boot guard and
/// `reload::reload`'s hot-reload guard, so a config edit that would silently
/// swap credentials, egress, and failure modes underneath a running provider
/// is refused in both places, not just at startup.
pub fn antigravity_migration_error(credential_exists: bool) -> Option<String> {
    (!credential_exists).then(|| {
        "provider `antigravity` is routed but has no credential. It is now the native HTTP \
         upstream, not the local `agy` CLI. Run `shunt login antigravity` to authenticate it, \
         or route to `antigravity-cli` to stay on the deprecated subprocess transport."
            .to_string()
    })
}

#[cfg(test)]
pub(crate) static ANTIGRAVITY_AUTH_FILE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use crate::config::{Config, ModelConfig, ProviderKind, RouteConfig, RoutePrefixConfig};

    use super::{
        antigravity_migration_error, routes_to_antigravity, routes_to_antigravity_cli,
        warn_if_routes_to_antigravity_cli,
    };

    struct BufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for BufferWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Turn the built-in `antigravity` provider into the one under test without
    /// hand-building a provider: `Config::default()` already seeds it.
    fn base() -> Config {
        let config = Config::default();
        assert_eq!(
            config
                .providers
                .get("antigravity")
                .map(|provider| provider.kind),
            Some(ProviderKind::Antigravity),
            "this test rests on the default config seeding an antigravity provider"
        );
        config
    }

    #[test]
    fn a_routed_native_antigravity_without_a_credential_refuses_to_start() {
        // Loud beats silent: routing to `antigravity` used to mean "run the
        // local `agy` binary", so booting green on the HTTP transport instead
        // would change credentials and egress underneath the operator.
        let message =
            antigravity_migration_error(false).expect("a missing credential must refuse the boot");
        assert!(message.contains("shunt login antigravity"), "{message}");
        // Both ways forward have to be named, including staying on the old
        // transport.
        assert!(message.contains("antigravity-cli"), "{message}");
    }

    #[test]
    fn a_present_credential_starts_normally() {
        assert_eq!(antigravity_migration_error(true), None);
    }

    #[test]
    fn the_two_antigravity_transports_are_detected_separately() {
        // Routing to one must not warm or gate the other: the CLI path spawns a
        // ~20s subprocess and the native path starts a version refresher.
        let mut config = base();
        config.server.default_provider = "antigravity".to_string();
        assert!(routes_to_antigravity(&config));
        assert!(!routes_to_antigravity_cli(&config));

        let mut config = base();
        config.server.default_provider = "antigravity-cli".to_string();
        assert!(routes_to_antigravity_cli(&config));
        assert!(!routes_to_antigravity(&config));
    }

    #[test]
    fn a_routed_cli_config_warns_about_the_deprecated_kind() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();

        let mut config = base();
        config.server.default_provider = "antigravity-cli".to_string();
        tracing::subscriber::with_default(subscriber, || {
            warn_if_routes_to_antigravity_cli(&config);
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(logs.contains("antigravity_cli"), "{logs}");
        assert!(logs.contains("deprecated"), "{logs}");
    }

    #[test]
    fn a_default_config_does_not_warn_about_the_deprecated_kind() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            warn_if_routes_to_antigravity_cli(&base());
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(logs.is_empty(), "{logs}");
    }

    #[test]
    fn default_config_does_not_route_to_antigravity() {
        // The provider exists but nothing selects it. This is the whole point:
        // presence is not routing.
        assert!(!routes_to_antigravity(&base()));
    }

    #[test]
    fn default_provider_pointing_at_antigravity_counts() {
        let mut config = base();
        config.server.default_provider = "antigravity".to_string();
        assert!(routes_to_antigravity(&config));
    }

    #[test]
    fn an_exact_route_counts() {
        let mut config = base();
        config.routes.push(RouteConfig {
            model: "gemini-3.1-pro".to_string(),
            provider: "antigravity".to_string(),
            upstream_model: None,
            effort: None,
            service_tier: None,
        });
        assert!(routes_to_antigravity(&config));
    }

    #[test]
    fn a_prefix_route_counts() {
        let mut config = base();
        config.route_prefixes.push(RoutePrefixConfig {
            prefix: "gemini-".to_string(),
            provider: "antigravity".to_string(),
        });
        assert!(routes_to_antigravity(&config));
    }

    #[test]
    fn a_model_upstream_map_counts() {
        let mut config = base();
        config.models.push(ModelConfig {
            id: "claude-gemini-via-agy".to_string(),
            display_name: None,
            upstream_model: Some(
                [("antigravity".to_string(), "gemini-3.1-pro".to_string())]
                    .into_iter()
                    .collect(),
            ),
        });
        assert!(routes_to_antigravity(&config));
    }
}
