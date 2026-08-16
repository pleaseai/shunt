//! Inbound client authentication for shared gateways (M4).
//!
//! Optional per-client tokens checked on discovery and routes where shunt
//! injects a server-side credential (every provider `auth` mode except
//! `passthrough`: `api_key`, `chatgpt_oauth`, `claude_oauth`, …). Those
//! checks accept the standard Anthropic client
//! credentials (`Authorization: Bearer`, `x-api-key`) in addition to the
//! dedicated token header. Passthrough inference routes are never checked —
//! the caller pays with their own credential. See `docs/m4-inbound-auth.md`.

use axum::http::{HeaderMap, HeaderName};

use crate::{config::AdminKeyring, gateway::GatewayAuth};

/// Resolved inbound-auth state: the header to inspect and the accepted
/// `name → token` pairs. Built once at startup from `[server.auth]` plus the
/// configured env var; absent entirely when inbound auth is not configured.
#[derive(Debug, Clone)]
pub struct InboundAuth {
    header: HeaderName,
    tokens: Vec<(String, String)>,
}

impl InboundAuth {
    pub fn new(header: HeaderName, tokens: Vec<(String, String)>) -> Self {
        Self { header, tokens }
    }

    pub fn header(&self) -> &HeaderName {
        &self.header
    }

    /// Check the request's configured inbound-auth header. Returns the matching
    /// client's name, or `None` when the header is missing or matches no
    /// configured token.
    pub fn authenticate(&self, headers: &HeaderMap) -> Option<&str> {
        self.authenticate_values(headers.get(&self.header).map(|value| value.as_bytes()))
    }

    /// Like [`Self::authenticate`] but also accepts an OpenAI-style
    /// `Authorization: Bearer <token>` credential, so a Codex CLI pointed at shunt
    /// with the standard `OPENAI_API_KEY` / `env_key` idiom (which Codex sends as a
    /// Bearer) authenticates the same as the configured token header. The `Bearer `
    /// scheme prefix is stripped before the constant-time compare; a non-`Bearer`
    /// scheme is ignored. Used by the inbound Codex endpoint; `x-api-key` is
    /// deliberately excluded because Codex never sends it.
    pub fn authenticate_bearer(&self, headers: &HeaderMap) -> Option<&str> {
        let bearer = bearer_token(headers);
        self.authenticate_values(
            headers
                .get(&self.header)
                .map(|value| value.as_bytes())
                .into_iter()
                .chain(bearer),
        )
    }

    /// Check every credential slot the Anthropic client protocol can carry a
    /// gate token in: the configured inbound-auth header, `Authorization:
    /// Bearer`, and `x-api-key`. Claude Code sends `ANTHROPIC_AUTH_TOKEN` as a
    /// Bearer and API keys as `x-api-key`, so a client pointed at a shared
    /// gateway authenticates with the credential it already sends — no extra
    /// custom header. Used by model discovery and by gated (injected-credential)
    /// `/v1/messages` inference routes.
    ///
    /// When several slots present valid tokens the dedicated header wins, then
    /// `Bearer`, then `x-api-key`: values are chained lowest-priority first
    /// because [`Self::authenticate_values`] keeps the last match.
    pub fn authenticate_client(&self, headers: &HeaderMap) -> Option<&str> {
        let bearer = bearer_token(headers);
        self.authenticate_values(
            headers
                .get("x-api-key")
                .map(|value| value.as_bytes())
                .into_iter()
                .chain(bearer)
                .chain(headers.get(&self.header).map(|value| value.as_bytes())),
        )
    }

    /// Compare every presented value against every configured token without an
    /// early exit, so timing does not reveal which token or credential matched.
    fn authenticate_values<'value>(
        &self,
        presented: impl IntoIterator<Item = &'value [u8]>,
    ) -> Option<&str> {
        let mut matched = None;
        for value in presented {
            if let Some(name) = self.authenticate_value(value) {
                matched = Some(name);
            }
        }
        matched
    }

    /// Constant-time check a raw presented value (not read from a header) against
    /// every configured token. Shared by [`Self::authenticate`] and the admin
    /// surface's login-form / token-header checks. Every entry is compared (no
    /// early exit) so timing does not reveal which matched.
    pub fn authenticate_value(&self, presented: &[u8]) -> Option<&str> {
        let mut matched = None;
        for (name, token) in &self.tokens {
            if constant_time_eq(presented, token.as_bytes()) {
                matched = Some(name.as_str());
            }
        }
        matched
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header, trimming the
/// scheme and surrounding whitespace. Returns `None` when the header is absent,
/// unparseable, or uses a non-`Bearer` scheme. Shared by
/// [`InboundAuth::authenticate_bearer`] and [`InboundAuth::authenticate_client`].
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&[u8]> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().split_once(' '))
        .and_then(|(scheme, token)| {
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.trim().as_bytes())
        })
}

/// Parse the tokens env value: comma-separated `name:token` pairs. Names and
/// tokens are trimmed; a token keeps everything after the first `:` (so it may
/// itself contain `:`). Wholly empty entries (trailing comma) are ignored.
pub fn parse_tokens(raw: &str) -> Result<Vec<(String, String)>, String> {
    let mut tokens: Vec<(String, String)> = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Do not echo the raw entry: a colonless value is often a bare token
        // pasted by mistake, and this message reaches startup logs.
        let (name, token) = entry.split_once(':').ok_or_else(|| {
            "an entry is not a name:token pair (expected \"name:token\")".to_string()
        })?;
        let name = name.trim();
        let token = token.trim();
        if name.is_empty() {
            return Err("entry has an empty client name".to_string());
        }
        if token.is_empty() {
            return Err(format!("client {name:?} has an empty token"));
        }
        if tokens.iter().any(|(existing, _)| existing == name) {
            return Err(format!("duplicate client name {name:?}"));
        }
        tokens.push((name.to_string(), token.to_string()));
    }
    if tokens.is_empty() {
        return Err("no client tokens configured".to_string());
    }
    Ok(tokens)
}

/// Constant-time equality: runs over the longer input and folds every byte
/// difference (and the length difference) into one accumulator, so timing does
/// not depend on where the first mismatch occurs.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// Which of shunt's own inbound credentials [`consumed_by`] matched. Exists
/// only so a caller can log *why* a slot was stripped without logging the
/// token value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsumedBy {
    GatewayJwt,
    StaticToken,
    AdminCredential,
}

/// Whether `value` — the raw contents of a header slot — is a credential shunt
/// itself consumes rather than the caller's own upstream credential, and which
/// one. Three kinds qualify: shunt's gateway JWT (checked as a bare token, no
/// `Bearer ` prefix), a configured static `[server.auth]` token, and a
/// `[server.admin]` credential (a `tokens_env`/`tokens_file` pair or either key
/// array — the read tier included, since a read key still reaches the admin
/// surface). Checked by value
/// per slot, not by whether *some* slot in the request authenticated the
/// caller, so a genuine upstream credential in one slot survives even when the
/// other slot holds a credential shunt consumed.
///
/// The gateway-JWT branch asks two different questions, deliberately in this
/// order: "does this authenticate right now" (`authenticate_token`), and only
/// if that fails, "is this shaped like a token shunt issued"
/// (`is_shunt_shaped_token`). A do-not-forward decision needs the second
/// question, not the first — an expired token, one minted by a sibling
/// instance under a different `public_url`, or one that no longer verifies
/// after a secret rotation is still shunt's own credential, and forwarding it
/// leaks the caller's identity and an offline HMAC oracle for `jwt_secret`.
/// Verifying first keeps the [`ConsumedBy::GatewayJwt`] label meaning "this
/// authenticated the caller" whenever it can; the shape check only widens
/// which non-authenticating tokens are still caught and stripped.
///
/// The admin branch is the mirror of
/// [`crate::admin::AdminAuth::authenticate_credential`], which accepts an admin
/// credential in `x-api-key` as well as in the configured admin header. Both
/// read the same `AdminKeyring`, so a value that can administer this gateway —
/// and an admin credential can provision upstream accounts — is never relayed
/// to the provider from a slot shunt would have authenticated it in.
///
/// Shared by discovery's passthrough header filtering
/// (`discovery/upstream.rs`) and inference failover's passthrough header
/// filtering (`proxy/failover.rs`), which independently apply it to the same
/// two slots (`authorization`, `x-api-key`) so the two request paths agree on
/// what "the caller's own credential" means.
pub(crate) fn consumed_by(
    value: &[u8],
    gateway_auth: Option<&GatewayAuth>,
    static_auth: Option<&InboundAuth>,
    admin_credentials: Option<&AdminKeyring>,
) -> Option<ConsumedBy> {
    let is_gateway_jwt = gateway_auth.is_some_and(|auth| {
        std::str::from_utf8(value).is_ok_and(|token| {
            let token = token.trim();
            auth.authenticate_token(token).is_some() || auth.is_shunt_shaped_token(token)
        })
    });
    if is_gateway_jwt {
        return Some(ConsumedBy::GatewayJwt);
    }
    if static_auth.is_some_and(|auth| auth.authenticate_value(value).is_some()) {
        return Some(ConsumedBy::StaticToken);
    }
    admin_credentials
        .is_some_and(|credentials| credentials.contains(value))
        .then_some(ConsumedBy::AdminCredential)
}

/// Like [`consumed_by`], but for a caller that only needs the yes/no answer.
pub(crate) fn is_consumed_by_shunt(
    value: &[u8],
    gateway_auth: Option<&GatewayAuth>,
    static_auth: Option<&InboundAuth>,
    admin_credentials: Option<&AdminKeyring>,
) -> bool {
    consumed_by(value, gateway_auth, static_auth, admin_credentials).is_some()
}

/// Evaluate the whole `Authorization` slot, which can carry a shunt-owned
/// credential in **two** shapes. Usually it is the `Bearer <token>` payload —
/// a gateway JWT, or a `[server.auth]` token sent the way Claude Code sends
/// `ANTHROPIC_AUTH_TOKEN`. But `[server.auth] header` is a free-form header
/// name (`InboundAuthConfig::resolve` only checks it parses as a `HeaderName`),
/// so an operator may set it to `authorization`; then
/// [`InboundAuth::authenticate_client`] authenticates off the *entire* header
/// value with no scheme prefix, and a caller passes the gate with a bare
/// `Authorization: <token>`. Checking only the Bearer payload finds nothing
/// consumed for that caller and relays their gate token upstream, so both
/// shapes are checked here. `[server.admin] header` is free-form in exactly the
/// same way (`InvalidAdminHeader` only checks it parses), so both shapes matter
/// for an admin credential too.
///
/// Returns `None` when the header is absent or holds the caller's own
/// credential.
pub(crate) fn authorization_consumed_by(
    headers: &HeaderMap,
    gateway_auth: Option<&GatewayAuth>,
    static_auth: Option<&InboundAuth>,
    admin_credentials: Option<&AdminKeyring>,
) -> Option<ConsumedBy> {
    let raw = headers.get("authorization")?;
    bearer_token(headers)
        .and_then(|token| consumed_by(token, gateway_auth, static_auth, admin_credentials))
        .or_else(|| consumed_by(raw.as_bytes(), gateway_auth, static_auth, admin_credentials))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    use super::{constant_time_eq, parse_tokens, InboundAuth};

    #[test]
    fn parses_name_token_pairs() {
        let tokens = parse_tokens("alice:tok-a, bob:tok-b").unwrap();
        assert_eq!(
            tokens,
            vec![
                ("alice".to_string(), "tok-a".to_string()),
                ("bob".to_string(), "tok-b".to_string()),
            ]
        );
    }

    #[test]
    fn token_keeps_everything_after_first_colon() {
        let tokens = parse_tokens("ci:v1:with:colons").unwrap();
        assert_eq!(
            tokens,
            vec![("ci".to_string(), "v1:with:colons".to_string())]
        );
    }

    #[test]
    fn trims_whitespace_and_ignores_trailing_comma() {
        let tokens = parse_tokens("  alice : tok-a ,").unwrap();
        assert_eq!(tokens, vec![("alice".to_string(), "tok-a".to_string())]);
    }

    #[test]
    fn rejects_malformed_entries() {
        assert!(parse_tokens("").is_err());
        assert!(parse_tokens("   ").is_err());
        assert!(parse_tokens("no-colon").is_err());
        assert!(parse_tokens(":token-without-name").is_err());
        assert!(parse_tokens("alice:").is_err());
        assert!(parse_tokens("alice:a,alice:b").is_err());
    }

    #[test]
    fn constant_time_eq_matches_semantics_of_eq() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn authenticate_returns_client_name_only_for_valid_token() {
        let auth = InboundAuth::new(
            HeaderName::from_static("x-shunt-token"),
            vec![
                ("alice".to_string(), "tok-a".to_string()),
                ("bob".to_string(), "tok-b".to_string()),
            ],
        );

        let mut headers = HeaderMap::new();
        assert_eq!(auth.authenticate(&headers), None);

        headers.insert("x-shunt-token", HeaderValue::from_static("tok-b"));
        assert_eq!(auth.authenticate(&headers), Some("bob"));

        headers.insert("x-shunt-token", HeaderValue::from_static("wrong"));
        assert_eq!(auth.authenticate(&headers), None);
    }

    #[test]
    fn authenticate_client_accepts_bearer_and_api_key_credentials() {
        let auth = InboundAuth::new(
            HeaderName::from_static("x-shunt-token"),
            vec![
                ("alice".to_string(), "tok-a".to_string()),
                ("bob".to_string(), "tok-b".to_string()),
            ],
        );

        // No credentials at all → rejected.
        assert_eq!(auth.authenticate_client(&HeaderMap::new()), None);

        // The configured inbound-auth header is accepted.
        let mut headers = HeaderMap::new();
        headers.insert("x-shunt-token", HeaderValue::from_static("tok-a"));
        assert_eq!(auth.authenticate_client(&headers), Some("alice"));

        // Claude Code's API-key idiom via `x-api-key`.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("tok-b"));
        assert_eq!(auth.authenticate_client(&headers), Some("bob"));

        // Claude Code's `ANTHROPIC_AUTH_TOKEN` idiom via `Authorization: Bearer`.
        let bearer = format!("Bearer {}", "tok-a");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(&bearer).unwrap());
        assert_eq!(auth.authenticate_client(&headers), Some("alice"));

        // A non-Bearer scheme is not treated as a credential.
        let basic = format!("Basic {}", "tok-a");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(&basic).unwrap());
        assert_eq!(auth.authenticate_client(&headers), None);

        // A wrong value on an otherwise-accepted source is rejected.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));
        assert_eq!(auth.authenticate_client(&headers), None);
    }

    #[test]
    fn authenticate_client_prefers_header_then_bearer_then_api_key() {
        let auth = InboundAuth::new(
            HeaderName::from_static("x-shunt-token"),
            vec![
                ("alice".to_string(), "tok-a".to_string()),
                ("bob".to_string(), "tok-b".to_string()),
            ],
        );

        // Dedicated header wins over a valid Bearer from another client.
        let mut headers = HeaderMap::new();
        headers.insert("x-shunt-token", HeaderValue::from_static("tok-a"));
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", "tok-b")).unwrap(),
        );
        assert_eq!(auth.authenticate_client(&headers), Some("alice"));

        // Bearer wins over a valid `x-api-key` from another client.
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", "tok-a")).unwrap(),
        );
        headers.insert("x-api-key", HeaderValue::from_static("tok-b"));
        assert_eq!(auth.authenticate_client(&headers), Some("alice"));

        // An invalid higher-priority slot does not mask a valid lower one.
        let mut headers = HeaderMap::new();
        headers.insert("x-shunt-token", HeaderValue::from_static("wrong"));
        headers.insert("x-api-key", HeaderValue::from_static("tok-b"));
        assert_eq!(auth.authenticate_client(&headers), Some("bob"));
    }

    #[test]
    fn authenticate_bearer_accepts_token_header_or_authorization_bearer() {
        let auth = InboundAuth::new(
            HeaderName::from_static("x-shunt-token"),
            vec![
                ("alice".to_string(), "tok-a".to_string()),
                ("bob".to_string(), "tok-b".to_string()),
            ],
        );

        // No credential at all → rejected.
        assert_eq!(auth.authenticate_bearer(&HeaderMap::new()), None);

        // The configured token header still works.
        let mut headers = HeaderMap::new();
        headers.insert("x-shunt-token", HeaderValue::from_static("tok-a"));
        assert_eq!(auth.authenticate_bearer(&headers), Some("alice"));

        // The OpenAI/Codex idiom: `OPENAI_API_KEY` / `env_key` → Authorization Bearer.
        let bearer = format!("Bearer {}", "tok-b");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(&bearer).unwrap());
        assert_eq!(auth.authenticate_bearer(&headers), Some("bob"));

        // A non-Bearer scheme is not treated as a credential.
        let basic = format!("Basic {}", "tok-a");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(&basic).unwrap());
        assert_eq!(auth.authenticate_bearer(&headers), None);

        // Unlike discovery, `x-api-key` is NOT accepted here (Codex never sends it).
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("tok-a"));
        assert_eq!(auth.authenticate_bearer(&headers), None);

        // A wrong Bearer value is rejected.
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer not-a-token"),
        );
        assert_eq!(auth.authenticate_bearer(&headers), None);
    }
}
