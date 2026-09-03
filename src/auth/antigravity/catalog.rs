//! The Antigravity backend's per-account model catalog.
//!
//! `POST {base_url}/v1internal:fetchAvailableModels` answers with
//! `{"models": {"<catalog id>": {...}, ...}}`, and the key set differs per
//! account and changes over time: one account is served
//! `gemini-3.8-flash-tiered` while another is still on
//! `gemini-3.8-flash-{high,medium,low}`. Which form exists decides which model
//! id [`crate::model::antigravity_request::antigravity_upstream_model`] may
//! send, so the catalog — not a compiled-in rule — is the authority.
//!
//! Discovery is best-effort by construction. Every failure path returns the
//! last known good set, or `None`, and the caller falls back to the
//! pre-catalog heuristic: a catalog outage must degrade the id shunt guesses,
//! never fail the operator's request.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// How long a fetched catalog is served without re-asking. The catalog changes
/// on the order of days (an account gained `-tiered` ids overnight), so ten
/// minutes keeps a rollout visible within one coffee break while costing at
/// most one extra request per host per that window.
const CATALOG_TTL: Duration = Duration::from_secs(600);
/// Whole-fetch bound, not just time-to-headers: this runs inline on the
/// request path, so a hung control plane must cost the request a known small
/// delay and then fall back, rather than the client's whole timeout budget.
const CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a *failed* discovery is remembered before it is retried.
///
/// Without this the entry a failure produces is indistinguishable from no
/// entry at all, so a backend that does not answer `fetchAvailableModels` —
/// a loopback proxy that forwards only `generateContent`, a revoked scope, a
/// control-plane blip — costs *every* subsequent request another inline fetch,
/// up to the full [`CATALOG_FETCH_TIMEOUT`], for as long as it stays down. A
/// minute is short enough that a recovered backend is picked up promptly and
/// long enough that the failure is paid once per window rather than per
/// request.
const CATALOG_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

struct CachedCatalog {
    /// When this entry was written — by a success or by a failure.
    refreshed_at: Instant,
    /// The last known good id set, or `None` when discovery has never
    /// succeeded for this account.
    ids: Option<Arc<BTreeSet<String>>>,
    /// Whether the write that produced `refreshed_at` was a successful fetch.
    /// A failed entry is retried after the shorter
    /// [`CATALOG_FAILURE_COOLDOWN`] rather than after [`CATALOG_TTL`].
    healthy: bool,
}

impl CachedCatalog {
    fn is_fresh(&self) -> bool {
        let ttl = if self.healthy {
            CATALOG_TTL
        } else {
            CATALOG_FAILURE_COOLDOWN
        };
        self.refreshed_at.elapsed() < ttl
    }
}

/// Keyed by [`cache_key`]: the *resolved* inference base URL plus the
/// account, so two providers pointed at the same backend under one account
/// share an entry, a production-pinned one shares the daily host's, and an
/// account swap never inherits the old one.
static CATALOG_CACHE: LazyLock<Mutex<HashMap<String, CachedCatalog>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One fetch slot per [`cache_key`], so a cold cache under load costs one
/// discovery request rather than one per in-flight request. The losers wait on the
/// winner and then re-read the cache, which is why the fetch is guarded rather
/// than skipped: without it every request in flight at process start — or at a
/// TTL boundary — POSTs `fetchAvailableModels` against a backend whose failure
/// mode for over-use is precisely the fake `429` this module exists to avoid.
static CATALOG_FETCH_SLOTS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The cache key for one backend under one account.
///
/// The catalog is per *account* — one account is served `-tiered` ids while
/// another still has the suffix form — so a key on the host alone serves the
/// previous account's catalog for up to [`CATALOG_TTL`] after
/// `shunt login antigravity` swaps the credential under a running gateway,
/// and every request in that window may pick an id the new account cannot
/// use. The account is identified by its Code Assist project: it is what the
/// backend itself scopes the request by (the envelope's `project`), it is
/// stable across the hourly access-token refresh — so the last-known-good set
/// survives a rotation that lands during a control-plane outage — and it
/// changes with the account. Only a credential with no project at all falls
/// back to a fingerprint of the bearer; that is a truncated SHA-256 so the
/// map never holds a token in the clear.
fn cache_key(base_url: &str, access_token: &str, project_id: &str) -> String {
    if !project_id.is_empty() {
        return format!("{base_url}#project:{project_id}");
    }
    let digest = Sha256::digest(access_token.as_bytes());
    let fingerprint = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{base_url}#token:{fingerprint}")
}

/// One catalog as handed to the resolver: the id set plus whether it came
/// from a fetch that succeeded just now. A set served after a failed refresh
/// is still worth a *positive* match — the account's ids did not change
/// because one request failed — but it cannot prove an id is gone, so the
/// resolver must not rewrite a pinned id on its say-so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSnapshot {
    pub ids: Arc<BTreeSet<String>>,
    pub fresh: bool,
}

impl std::ops::Deref for CatalogSnapshot {
    type Target = BTreeSet<String>;

    fn deref(&self) -> &Self::Target {
        &self.ids
    }
}

/// Drop entries no caller can reach any more. Every account swap — and every
/// token rotation of a project-less credential — leaves an entry behind under
/// the old key, so without this both maps grow for the life of the process. A fetch slot outlives its
/// cache entry only while someone still holds it — a first fetch in flight
/// has a slot and no entry yet — so a slot goes when its entry is gone *and*
/// this map holds the last reference.
fn evict_expired(cache: &mut HashMap<String, CachedCatalog>) {
    cache.retain(|_, entry| entry.refreshed_at.elapsed() < CATALOG_TTL * 2);
    // Lock order is cache → slots, here and nowhere else; `fetch_slot` and
    // `cached` each take one lock only.
    let mut slots = CATALOG_FETCH_SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slots.retain(|key, slot| cache.contains_key(key) || Arc::strong_count(slot) > 1);
}

fn cache() -> std::sync::MutexGuard<'static, HashMap<String, CachedCatalog>> {
    // Poison-tolerant: a panicking test holding the lock must not disable
    // catalog discovery for the rest of the process.
    CATALOG_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The fetch slot for one cache key, created on first use. Never held across
/// an await: only the `tokio` mutex it hands back is.
fn fetch_slot(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut slots = CATALOG_FETCH_SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(slots.entry(key.to_string()).or_default())
}

/// What the cache says about one key: serve this without a request (fresh
/// after a success, or the stale set inside a failure's cooldown), or refetch
/// and fall back on this set if the fetch fails.
enum Cached {
    Serve(Option<CatalogSnapshot>),
    Refetch(Option<Arc<BTreeSet<String>>>),
}

fn cached(key: &str) -> Cached {
    let cache = cache();
    match cache.get(key) {
        Some(entry) if entry.is_fresh() => {
            Cached::Serve(entry.ids.clone().map(|ids| CatalogSnapshot {
                ids,
                fresh: entry.healthy,
            }))
        }
        Some(entry) => Cached::Refetch(entry.ids.clone()),
        None => Cached::Refetch(None),
    }
}

/// The catalog ids for the backend `base_url` addresses, or `None` when
/// discovery has never succeeded for it.
///
/// Serves a cached set younger than [`CATALOG_TTL`] without a request. On a
/// miss or an expiry it fetches inline, bounded by [`CATALOG_FETCH_TIMEOUT`],
/// and on any failure falls back to the stale set it already had and records
/// the failure for [`CATALOG_FAILURE_COOLDOWN`], so an unreachable control
/// plane costs one bounded fetch per window rather than one per request.
/// Concurrent callers on the same backend and account share a single fetch,
/// and a different account never reads another's entry (see [`cache_key`]).
/// The snapshot says whether it came from a fetch that just succeeded; a set
/// served through a failure is `fresh: false`.
pub async fn catalog_ids(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
    project_id: &str,
) -> Option<CatalogSnapshot> {
    // Normalize here as well as at the call site: the cache key must not
    // depend on whether the caller resolved the host first, and production
    // does not serve `fetchAvailableModels` any more than it serves inference.
    let base_url = super::auth::inference_base_url(base_url);
    let key = cache_key(&base_url, access_token, project_id);

    let stale = match cached(&key) {
        Cached::Serve(snapshot) => return snapshot,
        Cached::Refetch(ids) => ids,
    };

    let slot = fetch_slot(&key);
    let _fetching = slot.lock().await;
    // Re-read under the slot: whoever held it may have just refreshed the
    // entry, in which case this caller owes the backend nothing.
    let stale = match cached(&key) {
        Cached::Serve(snapshot) => return snapshot,
        Cached::Refetch(ids) => ids.or(stale),
    };

    match fetch_catalog(client, &base_url, access_token).await {
        Some(ids) => {
            let ids = Arc::new(ids);
            let mut cache = cache();
            evict_expired(&mut cache);
            cache.insert(
                key,
                CachedCatalog {
                    refreshed_at: Instant::now(),
                    ids: Some(Arc::clone(&ids)),
                    healthy: true,
                },
            );
            Some(CatalogSnapshot { ids, fresh: true })
        }
        // Keep the stale entry rather than evicting it: a backend blip must
        // not cost the next ten minutes' worth of requests their catalog too.
        // Record the failure all the same, so the next request inside the
        // cooldown is served from here instead of repeating the fetch.
        None => {
            let mut cache = cache();
            evict_expired(&mut cache);
            cache.insert(
                key,
                CachedCatalog {
                    refreshed_at: Instant::now(),
                    ids: stale.clone(),
                    healthy: false,
                },
            );
            stale.map(|ids| CatalogSnapshot { ids, fresh: false })
        }
    }
}

async fn fetch_catalog(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
) -> Option<BTreeSet<String>> {
    let url = format!("{base_url}/v1internal:fetchAvailableModels");
    // The catalog is account-scoped, so it is addressed exactly as inference
    // is: the subscription bearer plus the Antigravity client fingerprint.
    let request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("User-Agent", super::version::user_agent())
        .bearer_auth(access_token)
        .json(&json!({}));

    let fetch = async {
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            // Drain through the shared helper rather than dropping the
            // response: the backend's own message is the only thing that
            // distinguishes a revoked scope from a relocated endpoint, and an
            // undrained body strands the pooled connection.
            let body = super::auth::diagnostic_body(CATALOG_FETCH_TIMEOUT, response).await;
            return Err(format!("backend answered {status}: {body}"));
        }
        response
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())
    };

    let body = match tokio::time::timeout(CATALOG_FETCH_TIMEOUT, fetch).await {
        Ok(Ok(body)) => body,
        Ok(Err(error)) => {
            tracing::warn!(
                %url,
                %error,
                "antigravity model catalog discovery failed; falling back to the model id shunt \
                 would have guessed without it"
            );
            return None;
        }
        Err(_) => {
            tracing::warn!(
                %url,
                timeout_secs = CATALOG_FETCH_TIMEOUT.as_secs(),
                "antigravity model catalog discovery timed out; falling back to the model id \
                 shunt would have guessed without it"
            );
            return None;
        }
    };

    let Some(models) = body.get("models").and_then(Value::as_object) else {
        tracing::warn!(
            %url,
            "antigravity model catalog response has no `models` object; falling back to the \
             model id shunt would have guessed without it"
        );
        return None;
    };

    // An empty object is a real answer, not a failure: it is cached like any
    // other, and the resolver treats "nothing matches" the same as no catalog.
    Some(models.keys().cloned().collect())
}

/// Seed the cache so a test can exercise catalog-driven resolution without a
/// backend. Key by a URL unique to the test — the cache is process-wide, and
/// tests run in parallel.
#[cfg(test)]
pub(crate) fn prime_for_test(base_url: &str, access_token: &str, project_id: &str, ids: &[&str]) {
    let ids = ids.iter().map(|id| (*id).to_string()).collect();
    cache().insert(
        cache_key(
            &super::auth::inference_base_url(base_url),
            access_token,
            project_id,
        ),
        CachedCatalog {
            refreshed_at: Instant::now(),
            ids: Some(Arc::new(ids)),
            healthy: true,
        },
    );
}

/// The fetch-slot keys, so a test can prove the slot map is swept.
#[cfg(test)]
fn slots_for_test() -> Vec<String> {
    CATALOG_FETCH_SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect()
}

/// Drop one host's cached catalogs under every credential, so a test can prove
/// the fetch path rather than an entry an earlier phase left behind.
#[cfg(test)]
pub(crate) fn clear_for_test(base_url: &str) {
    let prefix = format!("{}#", super::auth::inference_base_url(base_url));
    cache().retain(|key, _| !key.starts_with(&prefix));
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn catalog_body(ids: &[&str]) -> Value {
        let models = ids
            .iter()
            .map(|id| {
                (
                    (*id).to_string(),
                    json!({"model": "MODEL_PLACEHOLDER_M322"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({ "models": models })
    }

    #[tokio::test]
    async fn the_catalog_is_the_object_key_set_and_carries_the_client_identity() {
        // The entries' inner `model` values are internal placeholders the
        // backend 404s on, so the *keys* are the only wire ids — and the
        // catalog is account-scoped, so the request has to present the same
        // bearer and fingerprint inference does.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .and(header("authorization", "Bearer catalog-token"))
            .and(header_regex("user-agent", "^antigravity-hub/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(catalog_body(&[
                "gemini-3.8-flash-tiered",
                "claude-sonnet-4-6",
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let ids = catalog_ids(&reqwest::Client::new(), &server.uri(), "catalog-token", "")
            .await
            .expect("a 200 catalog must resolve");

        assert_eq!(
            ids.iter().map(String::as_str).collect::<Vec<_>>(),
            ["claude-sonnet-4-6", "gemini-3.8-flash-tiered"]
        );
        assert!(!ids.contains("MODEL_PLACEHOLDER_M322"));
        server.verify().await;
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn a_second_call_inside_the_ttl_makes_no_request() {
        // Discovery runs inline on the request path, so an uncached catalog
        // would add a round trip to every single message.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(catalog_body(&["gemini-3.6-flash-medium"])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let first = catalog_ids(&client, &server.uri(), "token", "")
            .await
            .unwrap();
        let second = catalog_ids(&client, &server.uri(), "token", "")
            .await
            .unwrap();

        assert_eq!(first, second);
        server.verify().await;
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn a_failed_fetch_yields_nothing_the_first_time_and_the_stale_set_afterwards() {
        // Nothing to fall back to means the resolver must hear `None` and use
        // its own heuristic. Once a catalog *has* been seen, a later outage
        // must not throw it away — the account's id set did not change just
        // because one request failed.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        clear_for_test(&server.uri());
        assert!(
            catalog_ids(&client, &server.uri(), "token", "")
                .await
                .is_none(),
            "a first fetch that fails has no set to serve"
        );

        prime_for_test(&server.uri(), "token", "", &["gemini-3.8-flash-tiered"]);
        // Age the entry past the TTL so the failing fetch is actually reached.
        {
            let mut cache = cache();
            let entry = cache
                .get_mut(&cache_key(
                    &super::super::auth::inference_base_url(&server.uri()),
                    "token",
                    "",
                ))
                .expect("the primed entry");
            entry.refreshed_at = Instant::now() - CATALOG_TTL - Duration::from_secs(1);
        }

        let stale = catalog_ids(&client, &server.uri(), "token", "")
            .await
            .expect("a failed refresh must serve the last known good set");

        assert!(stale.contains("gemini-3.8-flash-tiered"));
        assert!(!stale.fresh, "a set served through a failure is not proof");
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn a_failing_backend_is_asked_once_per_cooldown_not_once_per_request() {
        // Discovery runs inline on the request path, so a control plane that
        // does not answer `fetchAvailableModels` — a loopback proxy forwarding
        // only `generateContent`, a revoked scope — would otherwise add a
        // fetch, and up to CATALOG_FETCH_TIMEOUT, to every single request for
        // as long as it stays down.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        clear_for_test(&server.uri());
        for _ in 0..5 {
            assert!(catalog_ids(&client, &server.uri(), "token", "")
                .await
                .is_none());
        }

        server.verify().await;
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn concurrent_callers_on_a_cold_cache_share_one_fetch() {
        // Every request in flight at process start misses, and the backend's
        // failure mode for over-use is precisely the fake 429 this module
        // exists to route around.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(50))
                    .set_body_json(catalog_body(&["gemini-3.8-flash-tiered"])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        clear_for_test(&server.uri());
        let uri = server.uri();
        let fetches = (0..8).map(|_| {
            let client = client.clone();
            let uri = uri.clone();
            async move { catalog_ids(&client, &uri, "token", "").await }
        });
        let results = futures_util::future::join_all(fetches).await;

        assert!(results.iter().all(|ids| ids
            .as_ref()
            .is_some_and(|ids| ids.contains("gemini-3.8-flash-tiered"))));
        server.verify().await;
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn a_response_without_a_models_object_is_a_failure_not_an_empty_catalog() {
        // Treating a malformed body as "the account has no models" would make
        // every id resolve through the fallback while looking like a hit.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})))
            .mount(&server)
            .await;

        clear_for_test(&server.uri());
        assert!(
            catalog_ids(&reqwest::Client::new(), &server.uri(), "token", "")
                .await
                .is_none()
        );
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn a_swapped_account_never_reads_the_previous_accounts_catalog() {
        // `shunt login antigravity` can replace the credential under a running
        // gateway. The catalog is per account, so the entry the old account
        // filled must not answer for the new one — the first request under
        // the new project has to ask the backend, and must get *its* catalog.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .and(header("authorization", "Bearer new-account-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(catalog_body(&["gemini-3.8-flash-tiered"])),
            )
            .expect(1)
            .mount(&server)
            .await;
        clear_for_test(&server.uri());
        prime_for_test(
            &server.uri(),
            "old-account-token",
            "proj-old",
            &["gemini-3.8-flash-medium"],
        );

        let ids = catalog_ids(
            &reqwest::Client::new(),
            &server.uri(),
            "new-account-token",
            "proj-new",
        )
        .await
        .unwrap();

        assert!(ids.contains("gemini-3.8-flash-tiered"), "{ids:?}");
        assert!(!ids.contains("gemini-3.8-flash-medium"), "{ids:?}");
        assert!(ids.fresh);
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn a_refreshed_token_on_the_same_account_keeps_its_catalog() {
        // The access token rotates roughly hourly. That is the same account,
        // so the entry must survive it — keyed on the bearer, a rotation that
        // lands during a control-plane outage would throw away the
        // last-known-good set exactly when the fallback is needed, and every
        // routine refresh would cost a discovery request on top.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(catalog_body(&["gemini-3.8-flash-tiered"])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        clear_for_test(&server.uri());

        let first = catalog_ids(&client, &server.uri(), "token-before", "proj-same")
            .await
            .unwrap();
        let second = catalog_ids(&client, &server.uri(), "token-after", "proj-same")
            .await
            .unwrap();

        assert_eq!(first, second);
        server.verify().await;
        clear_for_test(&server.uri());
    }

    #[tokio::test]
    async fn an_abandoned_account_leaves_neither_a_cache_entry_nor_a_fetch_slot_behind() {
        // Every account swap is a new key. The cache evicts what nobody can
        // reach any more; the fetch slot map has to follow, or it grows by
        // one Arc per swap for the life of the process — the leak the
        // eviction exists to prevent, moved next door.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(catalog_body(&["gemini-3.8-flash-tiered"])),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        clear_for_test(&server.uri());
        let base = super::super::auth::inference_base_url(&server.uri());
        let old_key = cache_key(&base, "token", "proj-abandoned");

        catalog_ids(&client, &server.uri(), "token", "proj-abandoned")
            .await
            .unwrap();
        cache()
            .get_mut(&old_key)
            .expect("the old account's entry")
            .refreshed_at = Instant::now() - CATALOG_TTL * 2 - Duration::from_secs(1);
        assert!(slots_for_test().contains(&old_key), "the old slot exists");

        // The next insert under any key sweeps both maps.
        catalog_ids(&client, &server.uri(), "token", "proj-current")
            .await
            .unwrap();

        assert!(!cache().contains_key(&old_key), "the old entry is gone");
        assert!(!slots_for_test().contains(&old_key), "and so is its slot");
        assert!(
            slots_for_test().contains(&cache_key(&base, "token", "proj-current")),
            "the live key keeps its slot"
        );
        clear_for_test(&server.uri());
    }

    #[test]
    fn the_cache_key_is_the_project_and_never_the_bearer() {
        let by_project = cache_key("https://example.test", "ya29.secret-bearer", "proj-1");
        assert_eq!(by_project, "https://example.test#project:proj-1");
        assert_eq!(
            by_project,
            cache_key("https://example.test", "ya29.rotated-bearer", "proj-1"),
            "a token refresh on the same project is the same account"
        );
        assert_ne!(
            by_project,
            cache_key("https://example.test", "ya29.secret-bearer", "proj-2")
        );

        // No project: fall back to the bearer, fingerprinted.
        let by_token = cache_key("https://example.test", "ya29.secret-bearer", "");
        assert!(by_token.starts_with("https://example.test#token:"));
        assert!(!by_token.contains("secret"));
        assert_ne!(
            by_token,
            cache_key("https://example.test", "ya29.other-bearer", "")
        );
        assert_eq!(
            by_token,
            cache_key("https://example.test", "ya29.secret-bearer", "")
        );
    }
}
