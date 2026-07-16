pub mod auth;
pub mod login;

use std::sync::OnceLock;

/// Resolve the Cursor login/refresh base URL: the validated `SHUNT_CURSOR_BASE_URL`
/// override if set, else `default`. The override carries the Cursor
/// subscription/refresh token, so it is accepted only when it is an HTTPS Cursor
/// host — an off-origin or plaintext override is refused (logged once) and
/// `default` is used. Mirrors the config-load guard on `providers.<cursor>.base_url`
/// and `agent_base_url()`'s guard on the agent host.
pub fn resolve_base_url(default: impl Into<String>) -> String {
    cursor_base_override()
        .map(str::to_string)
        .unwrap_or_else(|| default.into())
}

/// The validated `SHUNT_CURSOR_BASE_URL` override, resolved once process-wide
/// (env read + validation + any warning happen a single time, not per request).
/// `None` when unset or rejected.
fn cursor_base_override() -> Option<&'static str> {
    static OVERRIDE: OnceLock<Option<String>> = OnceLock::new();
    OVERRIDE
        .get_or_init(|| validated_override(std::env::var("SHUNT_CURSOR_BASE_URL").ok().as_deref()))
        .as_deref()
}

/// Pure validation of a raw override value (no env read) so the accept/reject
/// rules are unit-testable: `Some(url)` only for an HTTPS Cursor host, else
/// `None` (a non-empty but rejected value is logged).
fn validated_override(raw: Option<&str>) -> Option<String> {
    let raw = raw.map(str::trim).filter(|value| !value.is_empty())?;
    match reqwest::Url::parse(raw) {
        Ok(url)
            if url.host_str().is_some_and(crate::config::host_is_cursor)
                && url.scheme() == "https" =>
        {
            Some(raw.to_string())
        }
        _ => {
            tracing::warn!(
                "ignoring SHUNT_CURSOR_BASE_URL {raw:?}: not an https cursor.sh/cursor.com host"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validated_override;

    #[test]
    fn unset_override_is_none() {
        assert_eq!(validated_override(None), None);
    }

    #[test]
    fn blank_override_is_none() {
        assert_eq!(validated_override(Some("")), None);
        assert_eq!(validated_override(Some("   ")), None);
    }

    #[test]
    fn https_cursor_override_is_accepted() {
        assert_eq!(
            validated_override(Some("https://api3.cursor.sh")).as_deref(),
            Some("https://api3.cursor.sh")
        );
        assert_eq!(
            validated_override(Some("https://cursor.com")).as_deref(),
            Some("https://cursor.com")
        );
    }

    #[test]
    fn override_is_trimmed() {
        assert_eq!(
            validated_override(Some("  https://api2.cursor.sh  ")).as_deref(),
            Some("https://api2.cursor.sh")
        );
    }

    #[test]
    fn non_https_override_is_rejected() {
        assert_eq!(validated_override(Some("http://cursor.sh")), None);
    }

    #[test]
    fn non_cursor_override_is_rejected() {
        assert_eq!(validated_override(Some("https://evil.example.com")), None);
    }

    #[test]
    fn garbage_override_is_rejected() {
        assert_eq!(validated_override(Some("not a url")), None);
    }
}
