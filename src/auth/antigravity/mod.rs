//! Antigravity subscription OAuth: credential store, login, and the client
//! version fingerprint the backend is addressed with.

use std::{collections::BTreeSet, env, path::PathBuf};

use crate::config::{AuthMode, Config, ProviderConfig, ProviderKind};

pub mod auth;
pub mod catalog;
pub mod login;
pub mod version;

/// shunt-owned Antigravity credential file: `$SHUNT_ANTIGRAVITY_AUTH_FILE`, else
/// `~/.shunt/antigravity-auth.json`. Written by `shunt login antigravity` and
/// refreshed by shunt alone — unlike the Gemini path, no other tool owns it.
pub fn default_antigravity_auth_path() -> PathBuf {
    env::var_os("SHUNT_ANTIGRAVITY_AUTH_FILE")
        // An empty override (e.g. `SHUNT_ANTIGRAVITY_AUTH_FILE=` from a
        // half-configured shell/CI environment) must fall back to the
        // default path rather than resolve to a bare empty PathBuf, which
        // would point at the process's current directory.
        .filter(|value| !value.is_empty())
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

/// Every provider name `config` can send a request to: its `default_provider`,
/// an exact `route`, a `route_prefix`, or a per-model `upstream_model` map
/// entry. Names are returned as written, including ones no `[providers.*]`
/// table defines — callers resolve them against the providers map themselves.
fn routed_provider_names(config: &Config) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    names.insert(config.server.default_provider.as_str());
    names.extend(config.routes.iter().map(|route| route.provider.as_str()));
    names.extend(
        config
            .route_prefixes
            .iter()
            .map(|prefix| prefix.provider.as_str()),
    );
    for model in &config.models {
        if let Some(map) = model.upstream_model.as_ref() {
            names.extend(map.keys().map(String::as_str));
        }
    }
    names
}

/// Whether `config` routes any request path to the given built-in-kind
/// provider.
fn routes_to_kind(config: &Config, kind: ProviderKind) -> bool {
    routed_provider_names(config).into_iter().any(|name| {
        config
            .providers
            .get(name)
            .is_some_and(|provider| provider.kind == kind)
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

/// Warn when a routed native Antigravity provider still pins
/// `base_url` at the production Code Assist host.
///
/// Production does not serve Antigravity inference — it answers the first
/// request with a fake-looking `429 RESOURCE_EXHAUSTED` — and docs before
/// 0.40.0 told operators to write exactly that host, so an upgraded config
/// keeps loading while every request fails. `auth::inference_base_url`
/// redirects the request itself; this says so out loud, once per provider, so
/// the operator can drop the key instead of living on a silent redirect.
/// Split from the call site so the warning is testable without spinning up the
/// server, like [`warn_if_routes_to_antigravity_cli`].
pub fn warn_if_antigravity_pinned_to_production(config: &Config) {
    for name in routed_provider_names(config) {
        let Some(provider) = config.providers.get(name) else {
            continue;
        };
        if !is_vetted_antigravity(provider) {
            continue;
        }
        if !auth::addresses_production_backend(&provider.base_url) {
            continue;
        }
        tracing::warn!(
            "providers.{name} base_url is pinned at https://cloudcode-pa.googleapis.com, which \
             does not serve Antigravity inference; requests go to \
             https://daily-cloudcode-pa.googleapis.com instead. Remove base_url or set it to the \
             daily host to silence this warning."
        );
    }
}

/// The refusal message for a routed native Antigravity provider with no
/// credential. Split from the filesystem probe so the message is testable
/// without depending on what happens to exist in the test environment's home
/// directory. Callers go through
/// [`routed_antigravity_credential_error`], which pairs it with the routing
/// predicate and the probe — so a config edit that would silently swap
/// credentials, egress, and failure modes underneath a running provider is
/// refused wherever a config is accepted: `check`, boot, and hot reload.
pub fn antigravity_migration_error(credential_exists: bool) -> Option<String> {
    (!credential_exists).then(|| {
        "provider `antigravity` is routed but has no credential. It is now the native HTTP \
         upstream, not the local `agy` CLI. Run `shunt login antigravity` to authenticate it, \
         or route to `antigravity-cli` to stay on the deprecated subprocess transport."
            .to_string()
    })
}

/// The routed-Antigravity credential guard as one predicate over a whole
/// config: `Some(message)` when `config` can send a request to a native
/// `antigravity` upstream and no credential exists to serve it with.
///
/// This is the single implementation every entry point runs — `shunt check`,
/// `main.rs`'s `serve()` boot path, and `reload::reload` — so a config that
/// `run` refuses to boot cannot pass `check` (issue #382).
///
/// Offline by construction: routing is read off the already-loaded config and
/// the credential is probed by existence alone. No token refresh, no project
/// discovery, no write to the credential file. That is what lets `shunt check`
/// run it while staying a static, network-free validation command.
pub fn routed_antigravity_credential_error(config: &Config) -> Option<String> {
    if !routes_to_antigravity(config) {
        return None;
    }
    antigravity_migration_error(default_antigravity_auth_path().exists())
}

/// The Code Assist host `shunt login antigravity` runs project discovery
/// against: the `base_url` of the Antigravity upstream the config routes to,
/// else of the only one it declares, else [`auth::DEFAULT_API_ENDPOINT`].
///
/// Looking a slot up by name alone is not enough, and that distinction is
/// load-bearing rather than defensive. `[providers.antigravity]` is only a
/// table name: an operator can point it at any other kind, and such a config
/// validates cleanly — the deprecated `antigravity_cli` transport keeps a
/// `http://localhost` placeholder under exactly this name, and
/// `kind = "responses"` with `auth = "passthrough"` and an arbitrary host is
/// accepted too, because the `AuthMode::AntigravityOauth` host guard in
/// [`Config::validate`] only fires for providers that actually use that auth
/// mode. Login mints a live subscription bearer and discovery sends it to
/// whichever host it is handed, so honoring an unvetted slot would carry the
/// token off-origin — or over plaintext to `localhost:80`. Only a
/// `kind = "antigravity"` provider with `auth = "antigravity_oauth"` has had
/// its `base_url` through that guard (https on the Code Assist host, or
/// loopback), which is the same guarantee the request path relies on in
/// `resolve_credential`.
pub fn login_base_url(config: Option<&Config>) -> String {
    let Some(config) = config else {
        return auth::DEFAULT_API_ENDPOINT.to_string();
    };

    // Prefer the upstreams the config can actually send a request to. A
    // non-ordered `[providers.*]` config keeps every built-in table merged in,
    // so the seeded `antigravity` slot exists — pointing at the default
    // backend — even
    // when the operator declared their own Antigravity provider under another
    // name and routed to that one. Picking by name alone would then discover
    // against production while the request path used the configured host,
    // which is the split issue #380 exists to close. Only when nothing
    // Antigravity-shaped is routed does the whole providers map become the
    // pool, so `shunt login antigravity` still honors a configured slot on a
    // config that has not been wired up to route to it yet.
    let routed = routed_provider_names(config);
    let vetted: Vec<(&str, &ProviderConfig)> = config
        .providers
        .iter()
        .filter(|(_, provider)| is_vetted_antigravity(provider))
        .map(|(name, provider)| (name.as_str(), provider))
        .collect();
    let routed_vetted: Vec<(&str, &ProviderConfig)> = vetted
        .iter()
        .copied()
        .filter(|(name, _)| routed.contains(name))
        .collect();
    let candidates = if routed_vetted.is_empty() {
        // Nothing Antigravity-shaped is routed, so there is no intent to read
        // off the routes — but the seeded `antigravity` table is still in the
        // map, and left at the built-in default it is not a declaration
        // either. Dropping it here cannot widen where the token can go: the
        // host it names is exactly the fallback this function returns when it
        // finds no signal at all. Keeping it would instead outvote the one
        // upstream the operator did declare, which is the same production/host
        // split for anyone who signs in before wiring up their routes.
        vetted
            .into_iter()
            .filter(|(name, provider)| {
                *name != "antigravity" || !auth::addresses_default_backend(&provider.base_url)
            })
            .collect()
    } else {
        routed_vetted
    };

    // Iteration is over a `BTreeMap`, so the candidate order is sorted key
    // order and every observation below is deterministic. Scanning is safe
    // here in a way that name lookup was not: each candidate satisfies the
    // exact predicate `Config::validate` vetted, so an unlucky pick can only
    // reach another host the operator themself pinned — never an arbitrary
    // one.
    match candidates.as_slice() {
        [] => auth::DEFAULT_API_ENDPOINT.to_string(),
        [(_, provider)] => provider.base_url.clone(),
        // Several qualify. The built-in name still disambiguates when it is
        // one of them: an operator who configured that slot named the upstream
        // login has always meant.
        several => {
            if let Some((_, provider)) = several.iter().find(|(name, _)| *name == "antigravity") {
                return provider.base_url.clone();
            }

            // Otherwise there is no operator-intended pick to infer — a
            // failover chain can hold both a debug proxy and the real backend.
            // Guessing would provision a project against a backend the
            // operator never chose, so say so and use the built-in default,
            // which is what login targeted before it read config at all.
            tracing::warn!(
                candidates = ?several.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                "several antigravity_oauth upstreams are configured and none is named \
                 `antigravity`; signing in against the default Code Assist endpoint. Name the \
                 one to log in to `antigravity` to discover the project against it."
            );
            auth::DEFAULT_API_ENDPOINT.to_string()
        }
    }
}

/// Whether `provider` is an Antigravity upstream whose `base_url` the
/// `AuthMode::AntigravityOauth` guard in [`Config::validate`] has vetted.
fn is_vetted_antigravity(provider: &ProviderConfig) -> bool {
    provider.kind == ProviderKind::Antigravity && provider.auth == AuthMode::AntigravityOauth
}

/// Process-env guard for tests that set `SHUNT_ANTIGRAVITY_AUTH_FILE`.
///
/// Deliberately the *same* mutex as [`crate::config::CONFIG_ENV_LOCK`] rather
/// than a second one. Both guard the single process environment against a
/// concurrent `Config::load`, and two independent mutexes do not exclude each
/// other: with a lock of its own, this family ran concurrently with
/// `an_env_only_legacy_antigravity_shape_is_rejected`, whose
/// `SHUNT_PROVIDERS__ANTIGRAVITY__BASE_URL` then leaked into a reload test's
/// `Config::load` and failed it with `AntigravityLegacyTableMissingAuth`
/// (~40% of filtered runs). Acquire it poison-tolerantly, per that constant's
/// documented convention.
#[cfg(test)]
pub(crate) use crate::config::CONFIG_ENV_LOCK as ANTIGRAVITY_AUTH_FILE_ENV_LOCK;

#[cfg(test)]
mod tests {
    use std::env;
    use std::io;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use crate::config::{
        AuthMode, Config, ModelConfig, ProviderKind, RouteConfig, RoutePrefixConfig,
    };

    use super::{
        antigravity_migration_error, default_antigravity_auth_path, login_base_url,
        routes_to_antigravity, routes_to_antigravity_cli, warn_if_antigravity_pinned_to_production,
        warn_if_routes_to_antigravity_cli, ANTIGRAVITY_AUTH_FILE_ENV_LOCK,
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

    /// Run `emit` with a capturing subscriber installed and return what it
    /// logged.
    fn captured_logs(emit: impl FnOnce()) -> String {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, emit);
        let logs = output.lock().unwrap().clone();
        String::from_utf8(logs).unwrap()
    }

    /// A production-pinned copy of the seeded Antigravity provider, declared
    /// under `name` — the shape pre-0.40.0 docs told operators to write.
    fn with_production_pinned_provider(config: &mut Config, name: &str) {
        let mut provider = config
            .providers
            .get("antigravity")
            .expect("the default config seeds an antigravity provider")
            .clone();
        provider.base_url = super::auth::API_ENDPOINT.to_string();
        config.providers.insert(name.to_string(), provider);
    }

    #[test]
    fn a_routed_production_pinned_antigravity_provider_warns() {
        // Production answers Antigravity inference with a fake 429, so a
        // config written against pre-0.40.0 docs loads and then fails every
        // request. The request path redirects it; the operator has to be told
        // which provider is being redirected.
        let mut config = base();
        with_production_pinned_provider(&mut config, "agy-prod");
        config.server.default_provider = "agy-prod".to_string();

        let logs = captured_logs(|| warn_if_antigravity_pinned_to_production(&config));

        assert!(logs.contains("providers.agy-prod"), "{logs}");
        assert!(logs.contains("daily-cloudcode-pa.googleapis.com"), "{logs}");
    }

    #[test]
    fn an_unrouted_production_pinned_antigravity_provider_does_not_warn() {
        // Every `Config::default()` seeds provider tables no route can reach,
        // so keying off the providers map alone would warn about upstreams
        // that never serve a request.
        let mut config = base();
        with_production_pinned_provider(&mut config, "agy-prod");

        let logs = captured_logs(|| warn_if_antigravity_pinned_to_production(&config));

        assert!(logs.is_empty(), "{logs}");
    }

    #[test]
    fn a_routed_antigravity_provider_on_the_daily_host_does_not_warn() {
        // The default `base_url` is already the daily host: routing to it is
        // the fixed configuration, and warning there would train operators to
        // ignore the message.
        let mut config = base();
        config.server.default_provider = "antigravity".to_string();

        let logs = captured_logs(|| warn_if_antigravity_pinned_to_production(&config));

        assert!(logs.is_empty(), "{logs}");
    }

    #[test]
    fn a_default_config_does_not_warn_about_a_production_pinned_base_url() {
        let logs = captured_logs(|| warn_if_antigravity_pinned_to_production(&base()));

        assert!(logs.is_empty(), "{logs}");
    }

    #[test]
    fn login_uses_a_configured_antigravity_base_url() {
        let mut config = base();
        config
            .providers
            .get_mut("antigravity")
            .expect("the default config seeds an antigravity provider")
            .base_url = "http://127.0.0.1:9999".to_string();

        assert_eq!(login_base_url(Some(&config)), "http://127.0.0.1:9999");
    }

    #[test]
    fn login_refuses_a_base_url_from_a_slot_that_is_not_the_antigravity_upstream() {
        // `[providers.antigravity]` is only a table name, and a config that
        // parks some other kind under it validates cleanly — the deprecated
        // CLI transport keeps a `http://localhost` placeholder there, and
        // `kind = "responses"` with `auth = "passthrough"` accepts any host at
        // all, because the AuthMode::AntigravityOauth host guard never fires
        // for it. Login mints a live subscription bearer and hands this host
        // straight to `discover_project`, so trusting the name alone would
        // send that token off-origin, or over plaintext to localhost:80. Only
        // a slot that guard actually vetted may redirect discovery.
        for (kind, auth) in [
            (ProviderKind::AntigravityCli, AuthMode::None),
            (ProviderKind::Responses, AuthMode::Passthrough),
            (ProviderKind::Antigravity, AuthMode::Passthrough),
            (ProviderKind::Gemini, AuthMode::GoogleOauth),
        ] {
            let mut config = base();
            let provider = config
                .providers
                .get_mut("antigravity")
                .expect("the default config seeds an antigravity provider");
            provider.kind = kind;
            provider.auth = auth;
            provider.base_url = "https://evil.example.com".to_string();

            assert_eq!(
                login_base_url(Some(&config)),
                super::auth::DEFAULT_API_ENDPOINT,
                "kind {kind:?} with auth {auth:?} must not redirect discovery"
            );
        }
    }

    #[test]
    fn login_finds_an_ordered_upstream_declared_under_another_name() {
        // An ordered `[[upstreams]]` config replaces the providers map
        // wholesale (`normalize_upstreams`), so there is no `antigravity` slot
        // left to look up — the upstream lives under the operator's own name.
        // A bare name lookup discovers against production here while the
        // request path uses the configured host, which is the split issue #380
        // exists to close.
        let mut config = base();
        let mut provider = config
            .providers
            .get("antigravity")
            .expect("the default config seeds an antigravity provider")
            .clone();
        provider.base_url = "http://127.0.0.1:9443".to_string();
        config.providers.remove("antigravity");
        config.providers.insert("agy-local".to_string(), provider);

        assert_eq!(login_base_url(Some(&config)), "http://127.0.0.1:9443");
    }

    #[test]
    fn login_refuses_to_guess_between_several_unnamed_antigravity_upstreams() {
        // A failover chain can hold both a debug proxy and the real backend.
        // With no built-in name to disambiguate there is no operator-intended
        // pick to infer, and guessing would provision a project against a
        // backend they never chose — so fall back to the built-in default
        // rather than take the first one.
        let mut config = base();
        let provider = config
            .providers
            .get("antigravity")
            .expect("the default config seeds an antigravity provider")
            .clone();
        config.providers.remove("antigravity");
        for (name, base_url) in [
            ("agy-a", "http://127.0.0.1:9443"),
            ("agy-b", "http://127.0.0.1:9444"),
        ] {
            let mut candidate = provider.clone();
            candidate.base_url = base_url.to_string();
            config.providers.insert(name.to_string(), candidate);
        }

        assert_eq!(
            login_base_url(Some(&config)),
            super::auth::DEFAULT_API_ENDPOINT
        );
    }

    #[test]
    fn login_prefers_the_built_in_name_when_it_also_qualifies() {
        // Determinism: with both a built-in slot and another qualifying
        // upstream, the pick must be the named one, not whichever sorts first.
        let mut config = base();
        let mut other = config
            .providers
            .get("antigravity")
            .expect("the default config seeds an antigravity provider")
            .clone();
        other.base_url = "http://127.0.0.1:9443".to_string();
        // Sorts before "antigravity", so a plain scan would return it.
        config.providers.insert("agy-aaa".to_string(), other);
        config
            .providers
            .get_mut("antigravity")
            .expect("still seeded")
            .base_url = "http://127.0.0.1:9999".to_string();

        assert_eq!(login_base_url(Some(&config)), "http://127.0.0.1:9999");
    }

    #[test]
    fn login_uses_the_only_declared_upstream_when_nothing_routes_to_it_yet() {
        // Signing in before wiring up routes is the ordinary setup order, and
        // the untouched built-in `antigravity` table is not a second
        // declaration — it still names the host login falls back to anyway.
        // Preferring it here would re-open the same production/host split for
        // every operator who logs in first.
        let mut config = base();
        let mut provider = config
            .providers
            .get("antigravity")
            .expect("the default config seeds an antigravity provider")
            .clone();
        provider.base_url = "http://127.0.0.1:9443".to_string();
        config.providers.insert("agy-local".to_string(), provider);
        assert!(
            config.providers.contains_key("antigravity"),
            "the untouched built-in slot must still be present for this to be a regression test"
        );
        assert!(
            !routes_to_antigravity(&config),
            "nothing may route to an Antigravity provider, or this exercises the routed path"
        );

        assert_eq!(login_base_url(Some(&config)), "http://127.0.0.1:9443");
    }

    #[test]
    fn login_treats_every_default_spelling_of_the_built_in_slot_as_no_signal() {
        // Whether the seeded `antigravity` table was left alone or spelled out
        // by hand, a slot that addresses the built-in default names exactly
        // the host login falls back to — so it never outvotes a declared
        // upstream. The check parses the URL rather than comparing bytes,
        // which is why the cased and trailing-slash spellings behave the same;
        // a byte compare would make the pick depend on capitalization, the
        // defect `addresses_default_backend` exists to prevent.
        for spelling in [
            "https://daily-cloudcode-pa.googleapis.com",
            "https://daily-cloudcode-pa.googleapis.com/",
            "https://Daily-CloudCode-PA.googleapis.com",
        ] {
            let mut config = base();
            let mut provider = config
                .providers
                .get("antigravity")
                .expect("the default config seeds an antigravity provider")
                .clone();
            provider.base_url = "http://127.0.0.1:9443".to_string();
            config.providers.insert("agy-local".to_string(), provider);
            config
                .providers
                .get_mut("antigravity")
                .expect("still seeded")
                .base_url = spelling.to_string();

            assert_eq!(
                login_base_url(Some(&config)),
                "http://127.0.0.1:9443",
                "built-in slot spelled {spelling}"
            );
        }
    }

    #[test]
    fn login_falls_back_to_the_default_backend_without_a_config() {
        // `shunt login antigravity` must still work when no config loads at
        // all — that is the whole reason main.rs treats the load as optional.
        assert_eq!(login_base_url(None), super::auth::DEFAULT_API_ENDPOINT);
    }

    /// A legacy `[providers.*]` config with the operator's own Antigravity
    /// upstream declared under another name and routed to, while the seeded
    /// built-in `antigravity` table is left untouched.
    fn legacy_config_routing_to(name: &str, base_url: &str, via: Routing) -> Config {
        let mut config = base();
        let mut provider = config
            .providers
            .get("antigravity")
            .expect("the default config seeds an antigravity provider")
            .clone();
        provider.base_url = base_url.to_string();
        config.providers.insert(name.to_string(), provider);

        match via {
            Routing::Default => config.server.default_provider = name.to_string(),
            Routing::Route => config.routes.push(RouteConfig {
                model: "gemini-3.1-pro".to_string(),
                provider: name.to_string(),
                upstream_model: None,
                effort: None,
                service_tier: None,
            }),
            Routing::Prefix => config.route_prefixes.push(RoutePrefixConfig {
                prefix: "gemini-".to_string(),
                provider: name.to_string(),
            }),
            Routing::UpstreamModel => config.models.push(ModelConfig {
                id: "gemini-3.1-pro".to_string(),
                display_name: None,
                upstream_model: Some(
                    [(name.to_string(), "gemini-3.1-pro-preview".to_string())]
                        .into_iter()
                        .collect(),
                ),
            }),
        }
        config
    }

    #[derive(Clone, Copy, Debug)]
    enum Routing {
        Default,
        Route,
        Prefix,
        UpstreamModel,
    }

    #[test]
    fn login_follows_the_routed_upstream_over_an_untouched_built_in_slot() {
        // `Config::default()` seeds an `antigravity` table pointing at
        // production, and a non-ordered `[providers.*]` config keeps every
        // built-in merged in. So the built-in slot qualifies even in a config
        // whose operator never configured it, and preferring it by name sent
        // discovery to production while requests went to the upstream they
        // actually routed to — the split issue #380 exists to close. Every
        // routing surface counts, because every one of them can carry the
        // request path to that provider.
        for via in [
            Routing::Default,
            Routing::Route,
            Routing::Prefix,
            Routing::UpstreamModel,
        ] {
            let config = legacy_config_routing_to("agy-local", "http://127.0.0.1:9443", via);
            assert!(
                config.providers.contains_key("antigravity"),
                "the untouched built-in slot must still be present for this to be a regression \
                 test, routed via {via:?}"
            );

            assert_eq!(
                login_base_url(Some(&config)),
                "http://127.0.0.1:9443",
                "routed via {via:?}"
            );
        }
    }

    #[test]
    fn login_still_prefers_the_built_in_slot_when_it_is_the_routed_one() {
        // The mirror of the case above: routing to the built-in name while
        // another Antigravity upstream sits unrouted in the map must keep
        // discovery on the built-in slot's configured host.
        let mut config =
            legacy_config_routing_to("agy-local", "http://127.0.0.1:9443", Routing::Default);
        config.server.default_provider = "antigravity".to_string();
        config
            .providers
            .get_mut("antigravity")
            .expect("still seeded")
            .base_url = "http://127.0.0.1:9999".to_string();

        assert_eq!(login_base_url(Some(&config)), "http://127.0.0.1:9999");
    }

    #[test]
    fn login_refuses_to_guess_between_several_routed_upstreams() {
        // Routing narrows the pool but does not always collapse it: a failover
        // config can route to two Antigravity upstreams at once. With no
        // built-in name among them there is still no pick to infer.
        let mut config =
            legacy_config_routing_to("agy-a", "http://127.0.0.1:9443", Routing::Default);
        let mut other = config
            .providers
            .get("agy-a")
            .expect("just inserted")
            .clone();
        other.base_url = "http://127.0.0.1:9444".to_string();
        config.providers.insert("agy-b".to_string(), other);
        config.route_prefixes.push(RoutePrefixConfig {
            prefix: "gemini-".to_string(),
            provider: "agy-b".to_string(),
        });
        config.providers.remove("antigravity");

        assert_eq!(
            login_base_url(Some(&config)),
            super::auth::DEFAULT_API_ENDPOINT
        );
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

    #[test]
    fn an_empty_auth_file_override_falls_back_to_the_default_path() {
        let _guard = ANTIGRAVITY_AUTH_FILE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = env::var_os("SHUNT_ANTIGRAVITY_AUTH_FILE");
        // A half-configured shell/CI environment (`SHUNT_ANTIGRAVITY_AUTH_FILE=`)
        // must not resolve to an empty PathBuf, which would point at the
        // process's current working directory rather than a real path.
        env::set_var("SHUNT_ANTIGRAVITY_AUTH_FILE", "");
        let path = default_antigravity_auth_path();
        match previous {
            Some(value) => env::set_var("SHUNT_ANTIGRAVITY_AUTH_FILE", value),
            None => env::remove_var("SHUNT_ANTIGRAVITY_AUTH_FILE"),
        }

        assert_ne!(path, PathBuf::new());
        assert!(
            path.ends_with("antigravity-auth.json"),
            "expected the default path, got {path:?}"
        );
    }

    #[test]
    fn a_non_empty_auth_file_override_is_honored() {
        let _guard = ANTIGRAVITY_AUTH_FILE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = env::var_os("SHUNT_ANTIGRAVITY_AUTH_FILE");
        env::set_var(
            "SHUNT_ANTIGRAVITY_AUTH_FILE",
            "/tmp/custom-antigravity-auth.json",
        );
        let path = default_antigravity_auth_path();
        match previous {
            Some(value) => env::set_var("SHUNT_ANTIGRAVITY_AUTH_FILE", value),
            None => env::remove_var("SHUNT_ANTIGRAVITY_AUTH_FILE"),
        }

        assert_eq!(path, PathBuf::from("/tmp/custom-antigravity-auth.json"));
    }
}
