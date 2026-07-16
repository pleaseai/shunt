pub mod auth;
pub mod login;

/// Validate a `SHUNT_CURSOR_BASE_URL` override against `default`, accepting it
/// only when it is an HTTPS Cursor host. The login/refresh endpoint carries the
/// Cursor subscription/refresh token, so an off-origin or plaintext override is
/// refused (logged) and falls back to `default` — mirroring the config-load
/// guard on `providers.<cursor>.base_url` and `agent_base_url()`'s guard on the
/// agent host.
pub(crate) fn resolve_base_url(default: impl Into<String>) -> String {
    let default = default.into();
    validated_base_url(
        std::env::var("SHUNT_CURSOR_BASE_URL").ok().as_deref(),
        &default,
    )
}

/// Pure core of [`resolve_base_url`] (no env read) so the accept/reject rules are
/// unit-testable.
fn validated_base_url(override_value: Option<&str>, default: &str) -> String {
    let Some(raw) = override_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return default.to_string();
    };
    match reqwest::Url::parse(raw) {
        Ok(url)
            if url.host_str().is_some_and(crate::config::host_is_cursor)
                && url.scheme() == "https" =>
        {
            raw.to_string()
        }
        _ => {
            tracing::warn!(
                "ignoring SHUNT_CURSOR_BASE_URL {raw:?}: not an https cursor.sh/cursor.com host; \
                 using {default}"
            );
            default.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validated_base_url;

    const DEFAULT: &str = "https://api2.cursor.sh";

    #[test]
    fn unset_override_uses_default() {
        assert_eq!(validated_base_url(None, DEFAULT), DEFAULT);
    }

    #[test]
    fn blank_override_uses_default() {
        assert_eq!(validated_base_url(Some(""), DEFAULT), DEFAULT);
        assert_eq!(validated_base_url(Some("   "), DEFAULT), DEFAULT);
    }

    #[test]
    fn https_cursor_override_is_accepted() {
        assert_eq!(
            validated_base_url(Some("https://api3.cursor.sh"), DEFAULT),
            "https://api3.cursor.sh"
        );
        assert_eq!(
            validated_base_url(Some("https://cursor.com"), DEFAULT),
            "https://cursor.com"
        );
    }

    #[test]
    fn override_is_trimmed_before_use() {
        assert_eq!(
            validated_base_url(Some("  https://api2.cursor.sh  "), DEFAULT),
            "https://api2.cursor.sh"
        );
    }

    #[test]
    fn non_https_override_is_rejected() {
        assert_eq!(
            validated_base_url(Some("http://cursor.sh"), DEFAULT),
            DEFAULT
        );
    }

    #[test]
    fn non_cursor_override_is_rejected() {
        assert_eq!(
            validated_base_url(Some("https://evil.example.com"), DEFAULT),
            DEFAULT
        );
    }

    #[test]
    fn garbage_override_is_rejected() {
        assert_eq!(validated_base_url(Some("not a url"), DEFAULT), DEFAULT);
    }
}
