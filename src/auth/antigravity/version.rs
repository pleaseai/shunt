//! Antigravity client version fingerprint.
//!
//! The backend is addressed as the Antigravity client, so requests carry that
//! client's `User-Agent`. Unlike shunt's other upstream fingerprints — which
//! pin a constant (`codex_cli_rs/…`, `connect-es/…`) — this one tracks the
//! shipping Antigravity release, because the version is published on an
//! auto-updater manifest and drifts.
//!
//! Three constraints shape the refresher, all of them lessons this repository
//! already paid for on the CLI transport:
//!
//! 1. **Never on the request path.** `40d6093` moved `agy` discovery off it
//!    after a ~20s subprocess landed in front of every startup. [`current`] is
//!    a lock read that never performs I/O; only [`spawn_refresher`] fetches.
//! 2. **Fail open.** A failed, slow, or malformed manifest response leaves the
//!    last known good version in place, and the compiled-in [`FALLBACK_VERSION`]
//!    is what a cold process uses. There is no state in which the User-Agent is
//!    empty or a request errors because of this module.
//! 3. **Bounded, with a TTL.** Issue #366 recorded what happens when a
//!    discovered catalogue is frozen for the process lifetime; a fingerprint
//!    frozen the same way ages into rejection. The fetch has a timeout and the
//!    value has an expiry.

use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Used until the first successful manifest fetch, and whenever one fails.
pub(crate) const FALLBACK_VERSION: &str = "2.2.1";
/// Platform segment of the Antigravity Hub user agent.
const HUB_PLATFORM: &str = "darwin/arm64";
/// Sent alongside the User-Agent on the onboarding call.
pub(crate) const GOOG_API_CLIENT: &str = "gl-node/22.21.1";
/// The Node client identifier the onboarding call presents.
pub(crate) const NODE_API_CLIENT: &str = "google-api-nodejs-client/10.3.0";

const MANIFEST_URL: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
const REFRESH_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum plausible length for a version string like `2.4.0` or
/// `2.4.0-beta.1`. Comfortably over anything a real release uses; this value
/// is formatted straight into the upstream User-Agent header, so a hijacked
/// or malformed manifest must not be able to inject an arbitrary string here.
const MAX_VERSION_LEN: usize = 32;

struct VersionCache {
    version: String,
    /// When the current value stops being considered fresh. `None` means the
    /// compiled-in fallback is in use and no successful fetch has happened.
    refreshed_at: Option<Instant>,
}

fn cache() -> &'static Mutex<VersionCache> {
    static CACHE: OnceLock<Mutex<VersionCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(VersionCache {
            version: FALLBACK_VERSION.to_string(),
            refreshed_at: None,
        })
    })
}

/// The version to advertise right now. Never blocks on I/O and never fails.
pub(crate) fn current() -> String {
    match cache().lock() {
        Ok(cache) => cache.version.clone(),
        // A poisoned lock must not take down request handling over a cosmetic
        // header; fall back rather than propagate.
        Err(poisoned) => poisoned.into_inner().version.clone(),
    }
}

/// The Antigravity Hub `User-Agent` for the current version.
pub(crate) fn user_agent() -> String {
    format!("antigravity-hub/{} {HUB_PLATFORM}", current())
}

/// The Node-client `User-Agent` the onboarding call presents.
pub(crate) fn node_user_agent() -> String {
    NODE_API_CLIENT.to_string()
}

fn store(version: String) {
    let mut guard = match cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.version = version;
    guard.refreshed_at = Some(Instant::now());
}

fn is_fresh() -> bool {
    let guard = match cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .refreshed_at
        .is_some_and(|at| at.elapsed() < REFRESH_TTL)
}

/// Start the background refresher. Idempotent: a second call is a no-op, so a
/// config reload cannot accumulate pollers.
pub fn spawn_refresher(client: reqwest::Client) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            if !is_fresh() {
                match fetch_version(&client, MANIFEST_URL).await {
                    Some(version) => {
                        tracing::debug!(version = %version, "Antigravity client version refreshed");
                        store(version);
                    }
                    None => {
                        // Deliberately not an error: the pinned fallback is a
                        // working value, so a manifest outage is a degraded
                        // fingerprint, not a broken provider.
                        tracing::debug!(
                            "Antigravity version manifest unavailable; keeping {}",
                            current()
                        );
                    }
                }
            }
            tokio::time::sleep(REFRESH_TTL / 2).await;
        }
    });
}

/// Fetch and parse the manifest. `None` on any failure — the caller keeps the
/// previous value.
async fn fetch_version(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = tokio::time::timeout(FETCH_TIMEOUT, client.get(url).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = tokio::time::timeout(FETCH_TIMEOUT, response.text())
        .await
        .ok()?
        .ok()?;
    parse_manifest_version(&body)
}

/// Read `version:` out of the updater manifest. The document is YAML, but only
/// this one scalar is needed, so it is scanned rather than parsed — adding a
/// YAML dependency for a single key is not worth it, and a malformed document
/// simply yields `None`.
///
/// Only a top-level (unindented) `version:` key is matched — the manifest's
/// own release version lives at the document root, and an indented one
/// belongs to a nested mapping (e.g. a per-file entry) that must not be
/// mistaken for it. The extracted value is validated by
/// [`is_plausible_version`] before being accepted: this value is formatted
/// straight into the upstream User-Agent header, so a compromised or
/// malformed manifest must not be able to smuggle arbitrary text there.
pub(crate) fn parse_manifest_version(body: &str) -> Option<String> {
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("version:") else {
            continue;
        };
        // Drop a trailing YAML comment before trimming quotes/whitespace, so
        // `version: 2.4.0 # notes` yields `2.4.0` rather than the whole tail.
        let rest = rest.split('#').next().unwrap_or("");
        let value = rest.trim().trim_matches(['"', '\''].as_ref()).trim();
        if is_plausible_version(value) {
            return Some(value.to_string());
        }
    }
    None
}

/// Whether `value` looks like a real version string rather than manifest
/// injection: non-empty, short, and built only from characters a version
/// string would plausibly use (this excludes control characters, whitespace,
/// and markup on top of rejecting anything overlong).
fn is_plausible_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VERSION_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_from_a_manifest() {
        let manifest = "version: 2.4.0\nfiles:\n  - url: Antigravity-2.4.0-arm64-mac.zip\n";
        assert_eq!(parse_manifest_version(manifest).as_deref(), Some("2.4.0"));
    }

    #[test]
    fn parses_a_quoted_version() {
        assert_eq!(
            parse_manifest_version("version: \"2.4.0\"\n").as_deref(),
            Some("2.4.0")
        );
    }

    #[test]
    fn manifest_without_a_version_yields_none() {
        // The fail-open contract: unparseable input must not produce an empty
        // version string that would then be formatted into the User-Agent.
        assert_eq!(parse_manifest_version("files:\n  - url: x.zip\n"), None);
        assert_eq!(parse_manifest_version("version:\n"), None);
        assert_eq!(parse_manifest_version(""), None);
    }

    #[test]
    fn an_indented_version_key_is_not_matched() {
        // Only the manifest's own top-level `version:` is the release version.
        // A nested `version:` under a per-file entry (or anything else
        // indented) must not be mistaken for it.
        let manifest = "files:\n  - url: x.zip\n    version: 9.9.9\nversion: 2.4.0\n";
        assert_eq!(parse_manifest_version(manifest).as_deref(), Some("2.4.0"));

        // With no top-level key at all, the indented one must not be
        // extracted either.
        let manifest = "files:\n  - url: x.zip\n    version: 9.9.9\n";
        assert_eq!(parse_manifest_version(manifest), None);
    }

    #[test]
    fn a_trailing_comment_is_stripped_from_the_value() {
        assert_eq!(
            parse_manifest_version("version: 2.4.0 # released today\n").as_deref(),
            Some("2.4.0")
        );
    }

    #[test]
    fn implausible_values_are_rejected() {
        // Control characters (here, a literal newline smuggled via a
        // multi-line YAML scalar folding into the scan) must not reach the
        // User-Agent header.
        assert_eq!(
            parse_manifest_version("version: 2.4.0\u{0007}\n"),
            None,
            "a value containing a control character must be rejected"
        );
        // An overlong value — this is a fingerprint header, not free text.
        let overlong = "a".repeat(64);
        assert_eq!(
            parse_manifest_version(&format!("version: {overlong}\n")),
            None,
            "an overlong value must be rejected"
        );
        // Arbitrary text with spaces/markup is not a version string.
        assert_eq!(
            parse_manifest_version("version: <script>alert(1)</script>\n"),
            None,
            "non-version-shaped text must be rejected"
        );
    }

    #[test]
    fn user_agent_carries_a_version_and_platform() {
        let agent = user_agent();
        assert!(
            agent.starts_with("antigravity-hub/"),
            "unexpected user agent: {agent}"
        );
        assert!(
            agent.contains(HUB_PLATFORM),
            "unexpected user agent: {agent}"
        );
        assert!(
            !agent.contains("antigravity-hub/ "),
            "version must never be empty: {agent}"
        );
    }

    #[tokio::test]
    async fn a_failed_fetch_returns_none_rather_than_an_empty_version() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert_eq!(
            fetch_version(&reqwest::Client::new(), &server.uri()).await,
            None
        );
        // The advertised version is unchanged by the failure.
        assert_eq!(current(), FALLBACK_VERSION);
    }
}
