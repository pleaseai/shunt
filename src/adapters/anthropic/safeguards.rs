//! Strip Claude Code's auto-mode safeguard protocol on non-first-party hosts.
//!
//! Auto mode (Claude Code 2.1.278) asks the API to run its permission
//! classifier server-side: the request carries the beta token
//! `dangerous-tool-use-2026-09-03` in `anthropic-beta` and a top-level
//! `safeguards` array. The two travel together — `api.anthropic.com` rejects the
//! field without the token (`400 safeguards: Extra inputs are not permitted`) —
//! and no other Anthropic-protocol host implements either.
//!
//! A 400 naming the field or the beta is the worst outcome available: Claude
//! Code denies every auto-mode tool use until the session is cleared. So an
//! Anthropic-compatible third party (Kimi, OpenRouter, DeepSeek, Z.ai, …) has
//! both removed before the request goes out, and the response side
//! (`crate::proxy::safeguards`) answers the client instead. The first-party host
//! keeps byte-for-byte passthrough — field, token and raw body untouched — so
//! its own classifier keeps answering.

use axum::http::{HeaderMap, HeaderValue};

use crate::request::RequestBody;

/// The only host known to implement the safeguard protocol.
const FIRST_PARTY_HOST: &str = "api.anthropic.com";

/// Every beta token that gates it shares this prefix; the dated suffix moves
/// with the client, so the family is matched rather than one spelling.
const SAFEGUARD_BETA_PREFIX: &str = "dangerous-tool-use-";

/// Drop the top-level `safeguards` field unless the provider is first-party.
pub(super) fn strip_unsupported_safeguards(body: &mut RequestBody, base_url: &str) {
    if is_first_party(base_url) {
        return;
    }
    // Read-only pre-check: a body without the field keeps its original bytes
    // rather than paying for a copy-on-write clone and a re-serialization.
    if body.json().get("safeguards").is_none() {
        return;
    }
    tracing::debug!(
        "stripping the auto-mode `safeguards` field for a non-first-party Anthropic host"
    );
    body.mutate(|request| {
        request
            .as_object_mut()
            .is_some_and(|request| request.remove("safeguards").is_some())
    });
}

/// Remove every `dangerous-tool-use-*` token from the outbound `anthropic-beta`
/// header unless the provider is first-party. The mirror of
/// [`strip_unsupported_safeguards`] — the pair has to leave together — and every
/// other token the client asked for survives.
pub(super) fn strip_safeguard_betas(headers: &mut HeaderMap, base_url: &str) {
    if is_first_party(base_url) {
        return;
    }
    // The header is a comma list, and a client may legally send it as several
    // field lines; `crate::headers::filtered` appends each one, so every field
    // reaches here. Fold them together before filtering — the single
    // remove-or-insert write-back below replaces the whole set, so it is only
    // correct against the aggregate.
    let beta = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    if beta.is_empty() || !beta.split(',').any(is_safeguard_beta) {
        return;
    }
    let kept = beta
        .split(',')
        .filter(|token| !is_safeguard_beta(token))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    if kept.is_empty() {
        headers.remove("anthropic-beta");
        return;
    }
    if let Ok(value) = HeaderValue::from_str(&kept) {
        headers.insert("anthropic-beta", value);
    }
}

fn is_safeguard_beta(token: &str) -> bool {
    token.trim().starts_with(SAFEGUARD_BETA_PREFIX)
}

/// True only for a provider whose `base_url` host is `api.anthropic.com`. An
/// unparseable or host-less base URL is treated as third-party: stripping a
/// field the host may not accept is recoverable, sending one it rejects is not.
fn is_first_party(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == FIRST_PARTY_HOST)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::{strip_safeguard_betas, strip_unsupported_safeguards};
    use crate::request::RequestBody;

    const FIRST_PARTY: &str = "https://api.anthropic.com";
    const THIRD_PARTY: &str = "https://api.moonshot.ai/anthropic";

    const RAW: &str = r#"{"model":"kimi-k2.7","safeguards":[{"type":"dangerous_tool_use","classifier_context":{"cwd":"/tmp"}}],"max_tokens":16}"#;

    fn headers(beta: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", beta.parse().unwrap());
        headers
    }

    /// Build the header from several field lines, as a client may legally send
    /// a comma-list header and as `crate::headers::filtered` forwards it.
    fn appended_headers(fields: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for field in fields {
            headers.append("anthropic-beta", field.parse().unwrap());
        }
        headers
    }

    #[test]
    fn a_third_party_host_loses_the_field_and_only_the_safeguard_beta() {
        let mut body = RequestBody::parse(RAW.as_bytes().to_vec()).unwrap();
        strip_unsupported_safeguards(&mut body, THIRD_PARTY);
        assert!(body.json().get("safeguards").is_none());
        assert_eq!(body.json()["max_tokens"], 16);
        assert!(!String::from_utf8(body.into_raw())
            .unwrap()
            .contains("safeguards"));

        let mut headers = headers(
            "claude-code-20250219,dangerous-tool-use-2026-09-03,interleaved-thinking-2025-05-14",
        );
        strip_safeguard_betas(&mut headers, THIRD_PARTY);
        assert_eq!(
            headers.get("anthropic-beta").unwrap(),
            "claude-code-20250219,interleaved-thinking-2025-05-14"
        );
    }

    #[test]
    fn a_third_party_host_drops_a_header_that_carried_nothing_else() {
        let mut headers = headers("dangerous-tool-use-2026-09-03");
        strip_safeguard_betas(&mut headers, THIRD_PARTY);
        assert!(headers.get("anthropic-beta").is_none());
    }

    #[test]
    fn repeated_header_fields_keep_every_other_beta() {
        let mut headers = appended_headers(&[
            "claude-code-20250219,dangerous-tool-use-2026-09-03",
            "interleaved-thinking-2025-05-14",
        ]);
        strip_safeguard_betas(&mut headers, THIRD_PARTY);
        assert_eq!(
            headers
                .get_all("anthropic-beta")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["claude-code-20250219,interleaved-thinking-2025-05-14"]
        );
    }

    #[test]
    fn a_safeguard_beta_in_a_later_header_field_is_still_stripped() {
        let mut headers = appended_headers(&[
            "claude-code-20250219",
            "dangerous-tool-use-2026-09-03,interleaved-thinking-2025-05-14",
        ]);
        strip_safeguard_betas(&mut headers, THIRD_PARTY);
        assert_eq!(
            headers
                .get_all("anthropic-beta")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["claude-code-20250219,interleaved-thinking-2025-05-14"]
        );
    }

    #[test]
    fn a_header_field_of_only_the_safeguard_beta_does_not_drop_its_siblings() {
        let mut headers =
            appended_headers(&["dangerous-tool-use-2026-09-03", "claude-code-20250219"]);
        strip_safeguard_betas(&mut headers, THIRD_PARTY);
        assert_eq!(
            headers
                .get_all("anthropic-beta")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["claude-code-20250219"]
        );
    }

    #[test]
    fn the_first_party_host_keeps_the_request_byte_for_byte() {
        let mut body = RequestBody::parse(RAW.as_bytes().to_vec()).unwrap();
        strip_unsupported_safeguards(&mut body, FIRST_PARTY);
        assert_eq!(String::from_utf8(body.into_raw()).unwrap(), RAW);

        let beta = "claude-code-20250219,dangerous-tool-use-2026-09-03";
        let mut headers = headers(beta);
        strip_safeguard_betas(&mut headers, FIRST_PARTY);
        assert_eq!(headers.get("anthropic-beta").unwrap(), beta);
    }

    #[test]
    fn a_request_without_safeguards_keeps_its_bytes_on_any_host() {
        let raw = r#"{"model":"kimi-k2.7","max_tokens":16}"#;
        let mut body = RequestBody::parse(raw.as_bytes().to_vec()).unwrap();
        strip_unsupported_safeguards(&mut body, THIRD_PARTY);
        assert_eq!(String::from_utf8(body.into_raw()).unwrap(), raw);

        let mut headers = headers("claude-code-20250219");
        strip_safeguard_betas(&mut headers, THIRD_PARTY);
        assert_eq!(
            headers.get("anthropic-beta").unwrap(),
            "claude-code-20250219"
        );
    }
}
