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

/// How long a fetched catalog is served without re-asking. The catalog changes
/// on the order of days (an account gained `-tiered` ids overnight), so ten
/// minutes keeps a rollout visible within one coffee break while costing at
/// most one extra request per host per that window.
const CATALOG_TTL: Duration = Duration::from_secs(600);
/// Whole-fetch bound, not just time-to-headers: this runs inline on the
/// request path, so a hung control plane must cost the request a known small
/// delay and then fall back, rather than the client's whole timeout budget.
const CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

struct CachedCatalog {
    fetched_at: Instant,
    ids: Arc<BTreeSet<String>>,
}

/// Keyed by the *resolved* inference base URL, so two providers pointed at the
/// same backend share one entry and a production-pinned one shares the daily
/// host's.
static CATALOG_CACHE: LazyLock<Mutex<HashMap<String, CachedCatalog>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache() -> std::sync::MutexGuard<'static, HashMap<String, CachedCatalog>> {
    // Poison-tolerant: a panicking test holding the lock must not disable
    // catalog discovery for the rest of the process.
    CATALOG_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The catalog ids for the backend `base_url` addresses, or `None` when
/// discovery has never succeeded for it.
///
/// Serves a cached set younger than [`CATALOG_TTL`] without a request. On a
/// miss or an expiry it fetches inline, bounded by [`CATALOG_FETCH_TIMEOUT`],
/// and on any failure falls back to the stale set it already had. Concurrent
/// callers may each fetch once; the cost of a duplicate discovery request is
/// far below the cost of holding a lock across an await on the request path.
pub async fn catalog_ids(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
) -> Option<Arc<BTreeSet<String>>> {
    // Normalize here as well as at the call site: the cache key must not
    // depend on whether the caller resolved the host first, and production
    // does not serve `fetchAvailableModels` any more than it serves inference.
    let base_url = super::auth::inference_base_url(base_url);

    let stale = {
        let cache = cache();
        match cache.get(&base_url) {
            Some(entry) if entry.fetched_at.elapsed() < CATALOG_TTL => {
                return Some(Arc::clone(&entry.ids));
            }
            Some(entry) => Some(Arc::clone(&entry.ids)),
            None => None,
        }
    };

    match fetch_catalog(client, &base_url, access_token).await {
        Some(ids) => {
            let ids = Arc::new(ids);
            cache().insert(
                base_url,
                CachedCatalog {
                    fetched_at: Instant::now(),
                    ids: Arc::clone(&ids),
                },
            );
            Some(ids)
        }
        // Keep the stale entry rather than evicting it: a backend blip must
        // not cost the next ten minutes' worth of requests their catalog too.
        None => stale,
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
            return Err(format!("backend answered {status}"));
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
pub(crate) fn prime_for_test(base_url: &str, ids: &[&str]) {
    let ids = ids.iter().map(|id| (*id).to_string()).collect();
    cache().insert(
        super::auth::inference_base_url(base_url),
        CachedCatalog {
            fetched_at: Instant::now(),
            ids: Arc::new(ids),
        },
    );
}

/// Drop one host's cached catalog, so a test can prove the fetch path rather
/// than an entry an earlier phase left behind.
#[cfg(test)]
pub(crate) fn clear_for_test(base_url: &str) {
    cache().remove(&super::auth::inference_base_url(base_url));
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

        let ids = catalog_ids(&reqwest::Client::new(), &server.uri(), "catalog-token")
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
        let first = catalog_ids(&client, &server.uri(), "token").await.unwrap();
        let second = catalog_ids(&client, &server.uri(), "token").await.unwrap();

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
            catalog_ids(&client, &server.uri(), "token").await.is_none(),
            "a first fetch that fails has no set to serve"
        );

        prime_for_test(&server.uri(), &["gemini-3.8-flash-tiered"]);
        // Age the entry past the TTL so the failing fetch is actually reached.
        {
            let mut cache = cache();
            let entry = cache
                .get_mut(&super::super::auth::inference_base_url(&server.uri()))
                .expect("the primed entry");
            entry.fetched_at = Instant::now() - CATALOG_TTL - Duration::from_secs(1);
        }

        let stale = catalog_ids(&client, &server.uri(), "token")
            .await
            .expect("a failed refresh must serve the last known good set");

        assert!(stale.contains("gemini-3.8-flash-tiered"));
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
        assert!(catalog_ids(&reqwest::Client::new(), &server.uri(), "token")
            .await
            .is_none());
        clear_for_test(&server.uri());
    }
}
