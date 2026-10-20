//! Which client sent a request, and whether it authenticated with a
//! gateway-issued credential (`docs/otlp-client-attribution.md`).
//!
//! The five per-request `shunt.*` series and the request span carry these two
//! fixed-vocabulary values so a dashboard can tell a server-derived series
//! apart from the client's own `claude_code.*` OTLP export without an operator
//! setting to keep in sync. Both are closed enumerations (3 × 2), so they cost
//! at most a sixfold multiple of the existing `(provider, model, …)` series.
//!
//! The raw header value is never exported: [`classify_client`] reduces it to
//! one of three fixed strings here and nothing else leaves this module.

use axum::http::HeaderMap;

/// Which client protocol shape the request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientKind {
    ClaudeCode,
    Codex,
    Other,
}

impl ClientKind {
    /// The fixed metric/span attribute value for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Other => "other",
        }
    }
}

/// The `client_kind` and `managed` attributes recorded for one request.
///
/// The key is `client_kind`, not `client`: `"client"` is already a value of
/// the `caller` attribute on `shunt.requests`/`shunt.latency` (a client turn
/// as opposed to a router's judge call), and the two must read apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientAttribution {
    pub client_kind: ClientKind,
    /// `true` only when the request authenticated with a gateway-issued JWT —
    /// the same credential that makes a client eligible for the pushed `OTEL_*`
    /// environment. Static `[server.auth]` tokens, admin keys, and
    /// unauthenticated passthrough are `false`.
    pub managed: bool,
}

impl ClientAttribution {
    /// The attribution for a call site that has no request headers to classify.
    pub const UNATTRIBUTED: Self = Self {
        client_kind: ClientKind::Other,
        managed: false,
    };

    /// Classify `headers` and pair the result with an already-derived
    /// `managed` decision (the credential is known to the auth layer, not to
    /// this module).
    pub fn new(headers: &HeaderMap, managed: bool) -> Self {
        Self {
            client_kind: classify_client(headers),
            managed,
        }
    }

    /// The fixed `client_kind` attribute value.
    pub fn client_kind_str(self) -> &'static str {
        self.client_kind.as_str()
    }
}

/// Identify the client from its request headers. Claude Code is checked first:
/// it sends both `x-app: cli` and a `claude-cli/` user agent, while a Codex CLI
/// sends `originator`, so a request carrying both is Claude Code's.
pub fn classify_client(headers: &HeaderMap) -> ClientKind {
    let user_agent = headers.get("user-agent").map(|value| value.as_bytes());
    let is_claude_code = headers
        .get("x-app")
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"cli"))
        || user_agent.is_some_and(|agent| starts_with_ignore_case(agent, b"claude-cli/"));
    if is_claude_code {
        return ClientKind::ClaudeCode;
    }
    if headers.contains_key("originator")
        || user_agent.is_some_and(|agent| starts_with_ignore_case(agent, b"codex_"))
    {
        return ClientKind::Codex;
    }
    ClientKind::Other
}

/// A user-agent product token is matched case-insensitively, like `x-app`.
fn starts_with_ignore_case(value: &[u8], prefix: &[u8]) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::{classify_client, ClientKind};
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn x_app_cli_is_claude_code() {
        assert_eq!(
            classify_client(&headers(&[("x-app", "CLI")])),
            ClientKind::ClaudeCode
        );
    }

    #[test]
    fn claude_cli_user_agent_is_claude_code() {
        assert_eq!(
            classify_client(&headers(&[(
                "user-agent",
                "claude-cli/2.1.0 (external, cli)"
            )])),
            ClientKind::ClaudeCode
        );
    }

    #[test]
    fn originator_header_is_codex() {
        assert_eq!(
            classify_client(&headers(&[("originator", "codex_cli_rs")])),
            ClientKind::Codex
        );
    }

    #[test]
    fn codex_user_agent_is_codex() {
        assert_eq!(
            classify_client(&headers(&[("user-agent", "codex_cli_rs/0.55.0")])),
            ClientKind::Codex
        );
    }

    #[test]
    fn user_agent_prefixes_match_case_insensitively() {
        assert_eq!(
            classify_client(&headers(&[("user-agent", "Claude-CLI/2.1.0")])),
            ClientKind::ClaudeCode
        );
        assert_eq!(
            classify_client(&headers(&[("user-agent", "Codex_CLI_rs/0.55.0")])),
            ClientKind::Codex
        );
    }

    #[test]
    fn an_unknown_client_is_other() {
        assert_eq!(
            classify_client(&headers(&[("user-agent", "curl/8.7.1")])),
            ClientKind::Other
        );
        assert_eq!(classify_client(&HeaderMap::new()), ClientKind::Other);
    }

    /// A request carrying both markers is Claude Code: the Claude Code check
    /// runs first, so an `originator` header cannot relabel it `codex`.
    #[test]
    fn x_app_wins_over_an_originator_header() {
        assert_eq!(
            classify_client(&headers(&[
                ("x-app", "cli"),
                ("originator", "codex_cli_rs")
            ])),
            ClientKind::ClaudeCode
        );
    }
}
