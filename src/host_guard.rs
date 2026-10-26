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
//! client can use, and on any bind once `allowed_hosts` is set. A request with no
//! `Host` at all (HTTP/1.0) is not a browser request and passes. `/` and
//! `/health` are exempt, as they are from the address allow list: they expose
//! nothing a rebinding page could use.

use std::{net::IpAddr, sync::Arc};

use axum::{
    extract::{Request, State},
    http::{header::HOST, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{
    concurrency::is_codex_request,
    config::Config,
    error::{into_openai_error_shape, ShuntError},
};

/// The `allowed_hosts` entry that turns the check off.
const ANY_HOST: &str = "*";

#[derive(Clone, Debug)]
pub(crate) struct HostGuard {
    /// Configured names accepted besides the built-in ones, normalized by
    /// [`host_name`].
    allowed: Arc<[String]>,
    codex_endpoint_enabled: bool,
}

impl HostGuard {
    /// The guard for this boot, or `None` when the check is off: on a
    /// non-loopback bind with no `allowed_hosts`, or when `allowed_hosts`
    /// contains `"*"`. Like the rest of `[server.access_control]`, it is fixed
    /// when the router is built.
    pub(crate) fn from_config(config: &Config, boot_is_loopback: bool) -> Option<Self> {
        let listed = &config.server.access_control.allowed_hosts;
        if listed.iter().any(|host| host == ANY_HOST) || (listed.is_empty() && !boot_is_loopback) {
            return None;
        }
        let public_urls = [
            config
                .server
                .gateway
                .as_ref()
                .map(|gateway| gateway.public_url.as_str()),
            config
                .server
                .admin
                .as_ref()
                .and_then(|admin| admin.oidc.as_ref())
                .map(|oidc| oidc.public_url.as_str()),
        ];
        let allowed = listed
            .iter()
            .filter_map(|host| host_name(host))
            .chain(public_urls.into_iter().flatten().filter_map(|url| {
                let url = url::Url::parse(url).ok()?;
                host_name(url.host_str()?)
            }))
            .collect();
        Some(Self {
            allowed,
            codex_endpoint_enabled: config.server.codex_endpoint.is_some(),
        })
    }

    /// Whether a request whose `Host` header holds `value` may be served.
    fn admits(&self, value: &str) -> bool {
        let Some(name) = host_name(value) else {
            return false;
        };
        let unbracketed = name
            .strip_prefix('[')
            .and_then(|name| name.strip_suffix(']'))
            .unwrap_or(&name);
        unbracketed.parse::<IpAddr>().is_ok()
            || name == "localhost"
            || name.ends_with(".localhost")
            || self.allowed.contains(&name)
    }
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

pub(crate) async fn enforce_host(
    State(guard): State<HostGuard>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/" | "/health") {
        return next.run(request).await;
    }
    let admitted = match request.headers().get(HOST) {
        Some(value) => value.to_str().is_ok_and(|value| guard.admits(value)),
        // HTTP/2 carries the name in `:authority`, which reaches the URI.
        None => request
            .uri()
            .authority()
            .is_none_or(|authority| guard.admits(authority.as_str())),
    };
    if admitted {
        return next.run(request).await;
    }
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<non-ascii>");
    tracing::warn!(
        host = %host.chars().take(255).collect::<String>(),
        "refused a request for a host this listener does not answer for; add it to [server.access_control] allowed_hosts if it is yours"
    );
    let response = ShuntError::new(
        StatusCode::FORBIDDEN,
        "permission_error",
        "this server does not answer for the requested host; an operator can add it to [server.access_control] allowed_hosts",
    )
    .into_response();
    if is_codex_request(request.uri(), guard.codex_endpoint_enabled) {
        into_openai_error_shape(response).await
    } else {
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(allowed: &[&str]) -> HostGuard {
        HostGuard {
            allowed: allowed.iter().map(|host| (*host).to_string()).collect(),
            codex_endpoint_enabled: false,
        }
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
            assert!(guard.admits(host), "{host} must be admitted");
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
        ] {
            assert!(!guard.admits(host), "{host:?} must be refused");
        }
    }

    #[test]
    fn admits_configured_names_case_insensitively_on_any_port() {
        let guard = guard(&["shunt.example.com"]);
        assert!(guard.admits("shunt.example.com"));
        assert!(guard.admits("Shunt.Example.com:8443"));
        assert!(!guard.admits("other.example.com"));
        assert!(!guard.admits("sub.shunt.example.com"));
    }

    /// The `public_url` hosts shunt already advertises are accepted without
    /// being listed: they are the names its own login redirects point at.
    #[test]
    fn configured_public_url_hosts_are_admitted() {
        let mut config = Config::default();
        config.server.gateway =
            Some(toml::from_str(r#"public_url = "https://Gateway.Example""#).unwrap());
        config.server.admin = Some(
            toml::from_str(
                r#"
                    [oidc]
                    public_url = "https://admin.example:8443"
                    issuer = "https://idp.example"
                    client_id = "shunt"
                "#,
            )
            .unwrap(),
        );
        let guard = HostGuard::from_config(&config, true).expect("loopback bind checks Host");
        assert!(guard.admits("gateway.example"));
        assert!(guard.admits("admin.example:8443"));
        assert!(!guard.admits("evil.example"));
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
}
