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
//!    a lock read that never performs I/O; only [`spawn_refresher`] and
//!    [`refresh_now`] fetch. The former runs in the background on a TTL and is
//!    never awaited; the latter is a bounded one-shot for `shunt login
//!    antigravity`, which resolves the Code Assist project during login
//!    itself (see `login.rs`) rather than on the request path, and so needs a
//!    fresh version *before* that call, not eventually.
//! 2. **Fail open.** A failed, slow, or malformed manifest response leaves the
//!    last known good version in place, and the compiled-in [`FALLBACK_VERSION`]
//!    is what a cold process uses. There is no state in which the User-Agent is
//!    empty or a request errors because of this module.
//! 3. **Bounded, with a TTL.** Issue #366 recorded what happens when a
//!    discovered catalogue is frozen for the process lifetime; a fingerprint
//!    frozen the same way ages into rejection. The fetch has a timeout and the
//!    value has an expiry.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
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
/// After a failed manifest fetch, retry soon rather than waiting out the full
/// [`REFRESH_TTL`] / 2 (3 hours). This is a cosmetic fingerprint header, not a
/// security-critical value: a short fixed interval — deliberately no
/// exponential backoff — keeps a real manifest outage from leaving the
/// advertised version stale for hours after it clears, without hammering the
/// endpoint on a transient blip.
const FAILED_FETCH_RETRY_INTERVAL: Duration = Duration::from_secs(60);
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

/// Guards [`spawn_refresher`] against starting a second background poller.
/// Module-level (rather than function-local) so `#[cfg(test)]` code can also
/// observe it and, in tests only, reset it — see [`is_refresher_started`]
/// and [`reset_refresher_started_for_test`]. An `AtomicBool` rather than the
/// `OnceLock<()>` this used to be: `OnceLock` has no safe way to un-set
/// itself, and the test-only reset needs exactly that.
static REFRESHER_STARTED: AtomicBool = AtomicBool::new(false);

/// Start the background refresher. Idempotent: a second call is a no-op, so a
/// config reload cannot accumulate pollers. `swap` rather than a
/// compare-and-exchange: with only two states, whichever concurrent caller's
/// swap observes the previous value as `false` is the one and only caller
/// that just performed the `false` -> `true` transition, so it is the sole
/// spawner -- every other caller, before or after, observes `true` and
/// returns.
pub fn spawn_refresher(client: reqwest::Client) {
    if REFRESHER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        loop {
            let sleep_duration = if is_fresh() {
                REFRESH_TTL / 2
            } else {
                let fetched = fetch_version(&client, MANIFEST_URL).await;
                match &fetched {
                    Some(version) => {
                        tracing::debug!(version = %version, "Antigravity client version refreshed");
                        store(version.clone());
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
                next_refresh_delay(fetched.as_deref())
            };
            tokio::time::sleep(sleep_duration).await;
        }
    });
}

/// One-shot version refresh for a caller that needs today's value *now*,
/// rather than eventually from [`spawn_refresher`]'s background poller.
///
/// `shunt login antigravity` is the one caller: it resolves the Code Assist
/// project during login itself (see `login.rs`), and login never starts
/// `spawn_refresher` (that only runs from the `serve`/`reload` paths), so
/// without this the discovery/onboarding calls it makes would always send
/// the compiled-in [`FALLBACK_VERSION`], never a refreshed one. Spawning the
/// background refresher instead and racing it would not help: it is
/// fire-and-forget by design, so login would have nothing to await.
///
/// Bounded by the same [`FETCH_TIMEOUT`] that already governs
/// [`fetch_version`]'s two stages (the request and the body read), so this
/// adds at most ~2 * `FETCH_TIMEOUT` = 20s to a login — the existing bound,
/// not a new one. Fail-open, matching the rest of this module: a failed
/// fetch leaves the cache untouched and this returns whatever was already in
/// it (the compiled-in fallback, on a cold process), exactly as if it had
/// never been called. Never blocks the login on a manifest outage.
pub(crate) async fn refresh_now(client: &reqwest::Client) -> String {
    refresh_now_from(client, MANIFEST_URL).await
}

/// [`refresh_now`] with an injectable URL, so tests can point it at a mock
/// server instead of the real manifest endpoint.
async fn refresh_now_from(client: &reqwest::Client, url: &str) -> String {
    if let Some(version) = fetch_version(client, url).await {
        store(version);
    }
    current()
}

/// How long to wait before the next refresh attempt: the full TTL half-life
/// after a successful fetch, or [`FAILED_FETCH_RETRY_INTERVAL`] after a failed
/// one so an outage does not leave the fingerprint stale for hours.
fn next_refresh_delay(fetched: Option<&str>) -> Duration {
    if fetched.is_some() {
        REFRESH_TTL / 2
    } else {
        FAILED_FETCH_RETRY_INTERVAL
    }
}

/// Fetch and parse the manifest. `None` on any failure — the caller keeps the
/// previous value.
async fn fetch_version(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = tokio::time::timeout(FETCH_TIMEOUT, client.get(url).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        // Drain the body (bounded by the same FETCH_TIMEOUT already governing
        // this call) rather than dropping the response un-drained, which would
        // strand the reqwest connection instead of returning it to the pool.
        // There is no error to enrich here — the caller only gets `None` — so
        // the drained body is logged at debug rather than folded into a
        // return value, matching this module's existing fail-open logging
        // for a manifest outage (the `None` branch in `spawn_refresher`).
        let status = response.status();
        let body = super::auth::diagnostic_body(FETCH_TIMEOUT, response).await;
        tracing::debug!(
            status = %status,
            body = %body,
            "Antigravity version manifest request rejected"
        );
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
        let rest = rest.trim_start();
        // A `#` only starts a YAML comment outside of quotes; inside a quoted
        // scalar it is part of the value. Splitting on `#` before trimming
        // quotes (the previous behavior) truncated a quoted value like
        // `"1.2#3"` down to `1.2` -- a corrupted-looking version silently
        // accepted as a plausible one. Extract the quoted scalar's own
        // content first, so a `#` inside it survives into `value` and is then
        // rejected by `is_plausible_version` (which does not allow `#`)
        // rather than silently truncated.
        let value = if let Some(quoted) = rest.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) if trailer_is_acceptable(&quoted[end + 1..]) => &quoted[..end],
                // Either unterminated, or something other than trailing
                // whitespace/comment follows the closing quote (e.g.
                // `"1.2" garbage`) -- salvaging the quoted prefix there would
                // accept a value out of a line that is not actually a clean
                // `version: "..."` entry. The manifest is untrusted input;
                // a malformed line must be rejected outright, not repaired.
                _ => continue,
            }
        } else if let Some(quoted) = rest.strip_prefix('\'') {
            match quoted.find('\'') {
                Some(end) if trailer_is_acceptable(&quoted[end + 1..]) => &quoted[..end],
                _ => continue,
            }
        } else {
            // Unquoted scalar: a `#` starts a comment here too, but only
            // when it is preceded by whitespace -- see `comment_start`. A
            // `#` glued directly onto the value (e.g. `1.2#3`) is part of
            // the scalar, not a comment marker, so it must survive into
            // `value` and be rejected by `is_plausible_version` rather than
            // silently cut off.
            match comment_start(rest) {
                Some(idx) => rest[..idx].trim(),
                None => rest.trim(),
            }
        };
        if is_plausible_version(value) {
            return Some(value.to_string());
        }
    }
    None
}

/// Whether the text following a quoted `version:` value's closing quote is
/// something a well-formed YAML scalar line would actually have there: only
/// whitespace, or whitespace then a `#` comment. Anything else -- most
/// notably trailing content after the quote, e.g. `version: "1.2" garbage`
/// -- means the closing quote [`parse_manifest_version`] found was not
/// actually the end of the line's value, so the "extracted" text is a
/// truncation of something malformed rather than a value to trust.
fn trailer_is_acceptable(trailer: &str) -> bool {
    let value = match comment_start(trailer) {
        Some(idx) => &trailer[..idx],
        None => trailer,
    };
    value.trim().is_empty()
}

/// Byte offset of the `#` that starts a YAML inline comment in `s`, if any.
/// YAML requires a `#` to be preceded by whitespace to begin a comment there
/// -- one glued directly onto preceding text (`"1.2"#x`, `1.2#x`) is part of
/// that text, not a comment marker. `trailer_is_acceptable`'s previous
/// `trim_start().starts_with('#')` check trimmed away any leading whitespace
/// before asking whether it existed, so `"1.2"# comment` (no separator) was
/// wrongly accepted; this only reports a `#` match when a whitespace byte
/// actually precedes it in `s`, never for one at the very start.
///
/// Byte-indexed rather than char-indexed: `#` and ASCII whitespace are both
/// single-byte, so a match index is always on a char boundary and safe to
/// slice on, even if the rest of `s` (untrusted manifest text) is not ASCII.
fn comment_start(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    (1..bytes.len()).find(|&i| bytes[i] == b'#' && bytes[i - 1].is_ascii_whitespace())
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

/// Test-only introspection for [`spawn_refresher`]'s guard: whether the
/// background poller has been started at all, anywhere in this process.
/// `reload.rs` uses this to assert that a hot reload which newly routes to
/// `antigravity` actually starts the refresher — not merely that the reload
/// call itself returns `Ok`.
#[cfg(test)]
pub(crate) fn is_refresher_started() -> bool {
    REFRESHER_STARTED.load(Ordering::Acquire)
}

/// Test-only: reset [`spawn_refresher`]'s guard so a test can observe
/// whether *its own* call path started the refresher, rather than
/// inheriting `true` left behind by an unrelated test elsewhere in this
/// binary. `REFRESHER_STARTED` is process-global by design (that is the
/// point of the guard), and Rust's default test harness runs every test in
/// this binary in parallel within one process, so without a reset the first
/// test anywhere to reach `spawn_refresher` would make
/// `is_refresher_started()` return `true` for every test after it,
/// regardless of what that later test's own reload path did.
///
/// Resetting does not stop a background poller a prior `spawn_refresher`
/// call may already have spawned — it only forgets that the guard was set,
/// so the next `spawn_refresher` call is free to spawn another one. That is
/// harmless in a short-lived test process, but it does mean a caller of
/// this function must serialize against every other test capable of
/// reaching `spawn_refresher`, so a reset here can never race a concurrent
/// `spawn_refresher` call from one of them. `reload.rs`'s antigravity tests
/// satisfy that by holding `ANTIGRAVITY_AUTH_FILE_ENV_LOCK` for their whole
/// body, which happens to be every test in the binary that can reach
/// `spawn_refresher` (`spawn_refresher`'s only other call site is
/// `main.rs`'s boot path, which no test exercises).
#[cfg(test)]
pub(crate) fn reset_refresher_started_for_test() {
    REFRESHER_STARTED.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that read or write the module-global version cache
    /// (`cache()`/`store()`/`current()`). Rust runs tests in the same
    /// process across a thread pool by default, and that cache has no
    /// per-test isolation, so a test that stores a real fetched version
    /// could otherwise race a concurrent test asserting the cache is still
    /// at its cold-start `FALLBACK_VERSION`. Tokio's mutex, not std's,
    /// because the guard needs to stay held across the `.await`s in the test
    /// bodies below.
    static VERSION_CACHE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    fn a_hash_inside_a_quoted_value_is_not_treated_as_a_comment_start() {
        // Splitting on `#` before trimming quotes used to truncate this to
        // `1.2`, a value that then passed `is_plausible_version` and was
        // silently accepted. The `#` is part of the quoted scalar, not a
        // comment, and `1.2#3` is not a plausible version string, so the
        // whole line must now be rejected rather than truncated.
        assert_eq!(
            parse_manifest_version("version: \"1.2#3\"\n"),
            None,
            "a `#` inside quotes must not truncate the value into a \
             different, plausible-looking one"
        );
    }

    #[test]
    fn a_quoted_version_with_trailing_garbage_is_rejected_not_truncated() {
        // The closing quote's position alone does not make what precedes it
        // trustworthy: `find('"')` also matches the quote that closes
        // `"1.2"` in a line that keeps going past it. Salvaging `1.2` there
        // would let a corrupted or tampered manifest line still update the
        // User-Agent -- the whole point of validating the manifest is to
        // distrust exactly this kind of malformed input, not repair it.
        assert_eq!(
            parse_manifest_version("version: \"1.2\" garbage\n"),
            None,
            "trailing content after the closing quote must reject the line, \
             not yield the truncated prefix"
        );
        assert_eq!(
            parse_manifest_version("version: '1.2' garbage\n"),
            None,
            "the same must hold for single-quoted values"
        );
    }

    #[test]
    fn a_quoted_version_followed_by_only_a_comment_is_still_accepted() {
        // Whitespace, or whitespace then a `#` comment, after the closing
        // quote is an ordinary well-formed line and must keep working.
        assert_eq!(
            parse_manifest_version("version: \"2.4.0\"   \n").as_deref(),
            Some("2.4.0"),
            "trailing whitespace after the closing quote must still parse"
        );
        assert_eq!(
            parse_manifest_version("version: \"2.4.0\" # released today\n").as_deref(),
            Some("2.4.0"),
            "a comment after the closing quote must still parse"
        );
    }

    #[test]
    fn a_comment_glued_directly_onto_a_quoted_value_is_rejected() {
        // YAML requires whitespace before `#` for it to start an inline
        // comment. `"1.2"# comment` has none between the closing quote and
        // the `#`, so that `#` is trailing garbage, not a comment marker --
        // the line must be rejected like any other malformed trailer, not
        // have the "comment" stripped and the value accepted anyway.
        assert_eq!(
            parse_manifest_version("version: \"1.2\"# comment\n"),
            None,
            "a `#` with no separating whitespace must not be treated as \
             starting a comment after a quoted value"
        );
    }

    #[test]
    fn a_comment_glued_directly_onto_an_unquoted_value_is_rejected() {
        // Same rule, unquoted branch: `2.4.0#comment` is one scalar
        // containing a `#`, not `2.4.0` plus a comment, so it must fail
        // `is_plausible_version` rather than be silently truncated down to
        // the plausible-looking `2.4.0`.
        assert_eq!(
            parse_manifest_version("version: 2.4.0#comment\n"),
            None,
            "a `#` with no separating whitespace must not be treated as \
             starting a comment after an unquoted value"
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
        let _guard = VERSION_CACHE_LOCK.lock().await;
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

    #[tokio::test]
    async fn refresh_now_leaves_the_cache_untouched_on_a_manifest_outage() {
        // The `shunt login antigravity` one-shot must be fail-open exactly
        // like the background refresher: a manifest outage returns whatever
        // was already cached instead of blocking or erroring.
        let _guard = VERSION_CACHE_LOCK.lock().await;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let before = current();
        let result = refresh_now_from(&reqwest::Client::new(), &server.uri()).await;
        assert_eq!(result, before);
        assert_eq!(current(), before);
    }

    #[tokio::test]
    async fn refresh_now_populates_the_cache_that_user_agent_reads() {
        // This is the property `shunt login antigravity` depends on: a
        // successful one-shot fetch must land in the same cache
        // `user_agent()`/`current()` read, not just be returned to the
        // caller, or the discover_project call login.rs makes right after it
        // would still see the stale fallback.
        let _guard = VERSION_CACHE_LOCK.lock().await;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("version: 9.9.9-test\n"))
            .mount(&server)
            .await;

        let result = refresh_now_from(&reqwest::Client::new(), &server.uri()).await;
        assert_eq!(result, "9.9.9-test");
        assert_eq!(current(), "9.9.9-test");
        assert!(user_agent().contains("9.9.9-test"));

        // Restore the cold-start value so later tests (and reruns of this
        // one) still observe the compiled-in fallback.
        store(FALLBACK_VERSION.to_string());
    }

    #[test]
    fn a_failed_fetch_schedules_a_short_retry_not_the_full_ttl() {
        // No exponential backoff, just a short fixed interval — but it must
        // actually be short relative to the TTL half-life, or a manifest
        // outage would still leave the fingerprint stale for hours.
        assert_eq!(next_refresh_delay(None), FAILED_FETCH_RETRY_INTERVAL);
        assert!(FAILED_FETCH_RETRY_INTERVAL < REFRESH_TTL / 2);
    }

    #[test]
    fn a_successful_fetch_schedules_the_full_ttl_half_life() {
        assert_eq!(next_refresh_delay(Some("2.4.0")), REFRESH_TTL / 2);
    }
}
