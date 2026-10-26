//! Inbound `Host` check: the DNS-rebinding guard.
//!
//! A page on `evil.example` can point its own name at `127.0.0.1` once it has
//! loaded, and from then on the browser treats shunt's port as that page's own
//! origin: same-origin requests, any headers, readable replies. The one thing the
//! browser still sends truthfully is the name it resolved, in `Host`. So a
//! listener that only answers for names an attacker cannot point at it closes
//! the hole without any client having to send anything new.
//!
//! The names that cannot be rebound are IP literals (no DNS is involved),
//! `localhost` and `*.localhost` (RFC 6761; browsers resolve them to loopback
//! themselves), and whatever the operator configured: the `public_url` hosts
//! shunt already advertises, plus `[server.access_control] allowed_hosts`.
//!
//! The check runs on a loopback bind, where those are the only names a real
//! client can use, and on any bind once `allowed_hosts` is set. A request with
//! neither a `Host` header nor a URI authority (HTTP/1.0) is not a browser
//! request and passes. `/` and `/health` are exempt, as they are from the
//! address allow list: they expose nothing a rebinding page could use.

use std::{net::IpAddr, sync::Arc};

use axum::{
    extract::{Request, State},
    http::{header::HOST, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{
    admin::AdminAuth,
    concurrency::is_codex_request,
    config::Config,
    error::{into_openai_error_shape, ShuntError},
    gateway::GatewayAuth,
    reload::{RuntimeState, SharedState},
};

/// The `allowed_hosts` entry that turns the check off.
pub(crate) const ANY_HOST: &str = "*";

/// The boot-fixed part of the check: whether it runs at all, and the
/// `allowed_hosts` names. Like the rest of `[server.access_control]`, both
/// need a restart to change.
#[derive(Clone, Debug)]
pub(crate) struct HostGuard {
    /// `allowed_hosts`, normalized by [`host_name`].
    allowed: Arc<[String]>,
    codex_endpoint_enabled: bool,
}

impl HostGuard {
    /// The guard for this boot, or `None` when the check is off: on a
    /// non-loopback bind with no `allowed_hosts`, or when `allowed_hosts`
    /// contains `"*"`.
    pub(crate) fn from_config(config: &Config, boot_is_loopback: bool) -> Option<Self> {
        let listed = &config.server.access_control.allowed_hosts;
        if listed.iter().any(|host| host == ANY_HOST) || (listed.is_empty() && !boot_is_loopback) {
            return None;
        }
        Some(Self {
            allowed: listed.iter().filter_map(|host| host_name(host)).collect(),
            codex_endpoint_enabled: config.server.codex_endpoint.is_some(),
        })
    }

    /// Whether `name` (normalized by [`host_name`]) is one shunt always
    /// answers for, or an `allowed_hosts` entry.
    fn admits(&self, name: &str) -> bool {
        let unbracketed = name
            .strip_prefix('[')
            .and_then(|name| name.strip_suffix(']'))
            .unwrap_or(name);
        unbracketed.parse::<IpAddr>().is_ok()
            || name == "localhost"
            || name.ends_with(".localhost")
            || self.allowed.iter().any(|host| host == name)
    }
}

/// The hosts the running gateway and admin login advertise, normalized by
/// [`host_name`]. Read from the resolved auth in the live state rather than
/// from the config: a reload re-resolves both, so a moved `public_url` is
/// followed at once, and it keeps the running gateway auth when
/// `[server.gateway]` is removed (its routes stay until a restart), which the
/// config alone would not show.
fn live_public_url_hosts(runtime: &RuntimeState) -> impl Iterator<Item = String> + '_ {
    let gateway = runtime.gateway_auth.as_deref().map(GatewayAuth::public_url);
    let admin = runtime
        .admin_auth
        .as_deref()
        .and_then(AdminAuth::oidc_public_url);
    [gateway, admin]
        .into_iter()
        .flatten()
        .filter_map(public_url_host)
}

/// The host of a resolved `public_url`, normalized by [`host_name`].
fn public_url_host(url: &str) -> Option<String> {
    host_name(url::Url::parse(url).ok()?.host_str()?)
}

/// The host part of a `Host` header value (or `allowed_hosts` entry), lowercased
/// and without its port or trailing dot; an IPv6 literal keeps its brackets.
/// `None` when it is not a valid authority or carries userinfo.
pub(crate) fn host_name(value: &str) -> Option<String> {
    let authority: axum::http::uri::Authority = value.parse().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let name = authority.host().trim_end_matches('.').to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// [`HostGuard`] plus the live state its `public_url` hosts are read from: the
/// state [`enforce_host`] runs with.
#[derive(Clone)]
pub(crate) struct HostCheck {
    pub(crate) guard: HostGuard,
    pub(crate) shared: SharedState,
}

pub(crate) async fn enforce_host(
    State(check): State<HostCheck>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/" | "/health") {
        return next.run(request).await;
    }
    // HTTP/2 carries the name in `:authority`, which reaches the URI.
    let requested = match request.headers().get(HOST) {
        Some(value) => Some(value.to_str().unwrap_or("<non-ascii>")),
        None => request
            .uri()
            .authority()
            .map(|authority| authority.as_str()),
    };
    let admitted = requested.is_none_or(|host| {
        host_name(host).is_some_and(|name| {
            check.guard.admits(&name)
                || live_public_url_hosts(&check.shared.load()).any(|host| host == name)
        })
    });
    if admitted {
        return next.run(request).await;
    }
    tracing::warn!(
        host = %requested.unwrap_or_default().chars().take(255).collect::<String>(),
        "refused a request for a host this listener does not answer for; add it to [server.access_control] allowed_hosts if it is yours"
    );
    let response = ShuntError::new(
        StatusCode::FORBIDDEN,
        "permission_error",
        "this server does not answer for the requested host; an operator can add it to [server.access_control] allowed_hosts",
    )
    .into_response();
    if is_codex_request(request.uri(), check.guard.codex_endpoint_enabled) {
        into_openai_error_shape(response).await
    } else {
        response
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.
mod tests {
    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;

    fn guard(allowed: &[&str]) -> HostGuard {
        HostGuard {
            allowed: allowed.iter().map(|host| (*host).to_string()).collect(),
            codex_endpoint_enabled: false,
        }
    }

    /// What the middleware decides for a `Host` value, before the live
    /// `public_url` hosts are consulted.
    fn admits(guard: &HostGuard, value: &str) -> bool {
        host_name(value).is_some_and(|name| guard.admits(&name))
    }

    #[test]
    fn admits_names_that_cannot_be_rebound() {
        let guard = guard(&[]);
        for host in [
            "127.0.0.1:3001",
            "127.0.0.1",
            "[::1]:3001",
            "[::1]",
            "localhost:3001",
            "LOCALHOST",
            "localhost.:3001",
            "app.localhost:3001",
            "10.0.0.5:8080",
        ] {
            assert!(admits(&guard, host), "{host} must be admitted");
        }
    }

    #[test]
    fn refuses_other_names_and_malformed_values() {
        let guard = guard(&[]);
        for host in [
            "evil.example:3001",
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "user@127.0.0.1:3001",
            "",
            "evil example",
            "<non-ascii>",
        ] {
            assert!(!admits(&guard, host), "{host:?} must be refused");
        }
    }

    #[test]
    fn admits_configured_names_case_insensitively_on_any_port() {
        let guard = guard(&["shunt.example.com"]);
        assert!(admits(&guard, "shunt.example.com"));
        assert!(admits(&guard, "Shunt.Example.com:8443"));
        assert!(!admits(&guard, "other.example.com"));
        assert!(!admits(&guard, "sub.shunt.example.com"));
    }

    #[test]
    fn public_url_host_normalizes_like_a_host_header() {
        assert_eq!(
            public_url_host("https://Gateway.Example").as_deref(),
            Some("gateway.example")
        );
        assert_eq!(
            public_url_host("https://admin.example:8443").as_deref(),
            Some("admin.example")
        );
        assert_eq!(
            public_url_host("http://[::1]:8787").as_deref(),
            Some("[::1]")
        );
        assert_eq!(public_url_host("not a url"), None);
    }

    #[test]
    fn the_check_is_off_without_a_list_off_loopback_and_with_the_wildcard() {
        let config = Config::default();
        assert!(HostGuard::from_config(&config, false).is_none());
        let mut listed = Config::default();
        listed.server.access_control.allowed_hosts = vec!["shunt.example.com".into()];
        assert!(HostGuard::from_config(&listed, false).is_some());
        listed.server.access_control.allowed_hosts.push("*".into());
        assert!(HostGuard::from_config(&listed, true).is_none());
    }

    #[test]
    fn host_name_normalizes_and_rejects_non_authorities() {
        assert_eq!(host_name("Example.COM:80").as_deref(), Some("example.com"));
        assert_eq!(host_name("[::1]:3001").as_deref(), Some("[::1]"));
        assert_eq!(host_name("example.com.").as_deref(), Some("example.com"));
        assert_eq!(host_name("https://example.com"), None);
        assert_eq!(host_name("example.com/path"), None);
        assert_eq!(host_name("user@example.com"), None);
    }

    /// The gateway's `public_url` host is admitted without being listed, and
    /// the check follows the running gateway through reloads: a moved
    /// `public_url` takes effect at once, and removing `[server.gateway]` keeps
    /// the running gateway (and so its host) until a restart, as `reload` does.
    #[tokio::test]
    async fn the_gateway_host_is_admitted_and_follows_reloads() {
        let _env_lock = crate::config::test_env_lock();
        let suffix = std::process::id();
        let secret_env = format!("SHUNT_HOST_GUARD_GATEWAY_SECRET_{suffix}");
        let users_env = format!("SHUNT_HOST_GUARD_GATEWAY_USERS_{suffix}");
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&users_env, "alice@example.com:secret");
        let dir = std::env::temp_dir().join(format!("shunt-host-guard-reload-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shunt.toml");
        let gateway_at = |url: &str| {
            format!(
                "[server.gateway]\npublic_url = \"{url}\"\njwt_secret_env = \"{secret_env}\"\nusers_env = \"{users_env}\"\n"
            )
        };

        std::fs::write(&path, gateway_at("https://gateway.example")).unwrap();
        let config = Config::load(Some(&path)).unwrap();
        let (router, shared, _state) = crate::server::build_router(config).unwrap();
        let status = |host: &'static str| {
            let router = router.clone();
            async move {
                let request = axum::http::Request::builder()
                    .uri("/routes")
                    .header("host", host)
                    .body(Body::empty())
                    .unwrap();
                router.oneshot(request).await.unwrap().status()
            }
        };
        assert_eq!(status("gateway.example").await, StatusCode::OK);
        assert_eq!(status("evil.example").await, StatusCode::FORBIDDEN);

        std::fs::write(&path, gateway_at("https://login.example")).unwrap();
        crate::reload::reload(&shared, Some(&path)).unwrap();
        assert_eq!(status("login.example").await, StatusCode::OK);
        assert_eq!(status("gateway.example").await, StatusCode::FORBIDDEN);

        std::fs::write(&path, "").unwrap();
        crate::reload::reload(&shared, Some(&path)).unwrap();
        assert_eq!(
            status("login.example").await,
            StatusCode::OK,
            "the retained gateway keeps its host"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var(secret_env);
        std::env::remove_var(users_env);
    }
}
