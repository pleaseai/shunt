//! Inbound client authentication for shared gateways (M4).
//!
//! Optional per-client tokens checked on discovery and routes where shunt
//! injects a server-side credential (`api_key` / `chatgpt_oauth`). Passthrough
//! inference routes are never checked — the caller pays with their own
//! credential. See `docs/m4-inbound-auth.md`.

use axum::http::{HeaderMap, HeaderName};

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

    /// Check credentials accepted by the Anthropic model-discovery protocol in
    /// addition to the configured inbound-auth header. Claude Code sends its
    /// discovery credential as either `Authorization: Bearer` or `x-api-key`.
    pub fn authenticate_discovery(&self, headers: &HeaderMap) -> Option<&str> {
        let bearer = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().split_once(' '))
            .and_then(|(scheme, token)| {
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then_some(token.trim().as_bytes())
            });
        self.authenticate_values(
            headers
                .get(&self.header)
                .map(|value| value.as_bytes())
                .into_iter()
                .chain(headers.get("x-api-key").map(|value| value.as_bytes()))
                .chain(bearer),
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
    fn authenticate_discovery_accepts_bearer_and_api_key_credentials() {
        let auth = InboundAuth::new(
            HeaderName::from_static("x-shunt-token"),
            vec![
                ("alice".to_string(), "tok-a".to_string()),
                ("bob".to_string(), "tok-b".to_string()),
            ],
        );

        // No credentials at all → rejected.
        assert_eq!(auth.authenticate_discovery(&HeaderMap::new()), None);

        // The configured inbound-auth header is accepted.
        let mut headers = HeaderMap::new();
        headers.insert("x-shunt-token", HeaderValue::from_static("tok-a"));
        assert_eq!(auth.authenticate_discovery(&headers), Some("alice"));

        // Claude Code's discovery credential via `x-api-key`.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("tok-b"));
        assert_eq!(auth.authenticate_discovery(&headers), Some("bob"));

        // Claude Code's discovery credential via `Authorization: Bearer`.
        let bearer = format!("Bearer {}", "tok-a");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(&bearer).unwrap());
        assert_eq!(auth.authenticate_discovery(&headers), Some("alice"));

        // A non-Bearer scheme is not treated as a discovery credential.
        let basic = format!("Basic {}", "tok-a");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_str(&basic).unwrap());
        assert_eq!(auth.authenticate_discovery(&headers), None);

        // A wrong value on an otherwise-accepted source is rejected.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));
        assert_eq!(auth.authenticate_discovery(&headers), None);
    }
}
