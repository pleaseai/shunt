//! Per-issuer JWKS state for [`super::verify`]: fetching, caching, expiry, and
//! the refetch floor. Split out of the parent module, which keeps the claim
//! checks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use serde::Deserialize;

use super::{validate_endpoint, JwtIssuerRule};

/// The same 10s budget `crate::gateway::idp_client` gives discovery, token, and
/// userinfo requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Floor between two JWKS fetches for one issuer. An unknown `kid` triggers at
/// most one refetch per window, so a caller cannot use forged `kid` values to
/// make shunt hammer the issuer's JWKS endpoint.
const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(60);

/// How long a fetched key set is trusted. Without a ceiling a cached `kid`
/// would verify for the whole process lifetime, so a key the issuer withdrew
/// (a compromised signing key being the case that matters) would keep
/// authenticating tokens until restart. Past this age the set is refetched
/// before it answers, and a successful refetch replaces it outright.
const KEY_SET_MAX_AGE: Duration = Duration::from_secs(300);

/// Cap on a JWKS (or discovery) document. Both are fetched from a configured,
/// operator-chosen origin, so this is a runaway guard rather than a defence
/// against a hostile peer.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// Process-lifetime, per-issuer JWKS state. Held on `AppState` alongside
/// `admin_stores` / `gateway_stores` rather than on the hot-reloadable
/// `InboundAuth`, so a config reload that leaves an entry unchanged does not
/// throw its keys away and refetch.
///
/// The outer `Mutex` is held only long enough to look up an issuer's entry; the
/// per-issuer `tokio::sync::Mutex` is held across the network fetch, so
/// concurrent requests for one issuer collapse into a single fetch while a
/// different issuer proceeds untouched. That is the isolation the design
/// requires: one issuer's outage must not deny the others. A `kid` found in a
/// key set younger than the max age is answered from the entry's snapshot
/// without touching that fetch lock, so a slow fetch (one a forged `kid` can
/// trigger every refetch window) never stalls requests the cache can answer.
pub struct JwksCache {
    client: reqwest::Client,
    issuers: Mutex<HashMap<CacheKey, Arc<IssuerEntry>>>,
    max_age: Duration,
    min_refetch: Duration,
}

/// An issuer plus the `jwks_url` configured for it. Keyed on both because two
/// entries for one issuer may name different JWKS endpoints, and a reload may
/// change the endpoint: either way the old endpoint's keys must not answer.
type CacheKey = (String, Option<String>);

#[derive(Default)]
struct IssuerEntry {
    /// The last successfully fetched key set. A plain `std` lock, never held
    /// across an `.await`: readers take a clone and leave, so the fast path
    /// does not queue behind `fetch`. Only the holder of `fetch` writes it.
    snapshot: Mutex<Option<KeySnapshot>>,
    /// Serialises fetches for this issuer (single-flight).
    fetch: tokio::sync::Mutex<FetchState>,
}

#[derive(Clone)]
struct KeySnapshot {
    keys: Arc<JwkSet>,
    /// When `keys` was fetched; drives [`KEY_SET_MAX_AGE`].
    fetched_at: Instant,
}

#[derive(Default)]
struct FetchState {
    /// The discovered `jwks_uri` the current key set was fetched from.
    /// Recorded only after a successful fetch, so a discovery outage falls
    /// back to an endpoint that has actually served keys.
    jwks_url: Option<String>,
    /// When a fetch was last *attempted*, successful or not, so a failing
    /// issuer is rate-limited exactly like a succeeding one.
    last_fetch: Option<Instant>,
}

impl IssuerEntry {
    fn snapshot(&self) -> Option<KeySnapshot> {
        self.snapshot
            .lock()
            .expect("inbound JWKS snapshot lock poisoned")
            .clone()
    }
}

/// A JWKS could not be produced. Deliberately opaque: the reason is logged, not
/// returned, so a caller cannot probe an issuer's reachability through response
/// differences.
#[derive(Debug)]
pub struct JwksUnavailable;

impl Default for JwksCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JwksCache {
    pub fn new() -> Self {
        Self::with_limits(KEY_SET_MAX_AGE, MIN_REFETCH_INTERVAL)
    }

    /// A cache whose key sets expire after `max_age`. The refetch floor is
    /// capped at `max_age` so an expired set can always be refreshed; with the
    /// production [`KEY_SET_MAX_AGE`] the floor is unchanged. Test-only.
    #[cfg(test)]
    pub(super) fn with_max_age(max_age: Duration) -> Self {
        Self::with_limits(max_age, MIN_REFETCH_INTERVAL.min(max_age))
    }

    /// A cache with explicit limits; the tests use it to make an unknown-`kid`
    /// refetch due while the cached set is still fresh.
    pub(super) fn with_limits(max_age: Duration, min_refetch: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("JWKS HTTP client configuration is valid"),
            issuers: Mutex::new(HashMap::new()),
            max_age,
            min_refetch,
        }
    }

    /// The key for `kid`, fetching or refetching this issuer's JWKS as needed.
    ///
    /// `Ok(None)` means "this issuer has usable keys and none of them is `kid`"
    /// — a `401`. `Err` means "no usable keys at all" — a `503`.
    pub(super) async fn key_for(
        &self,
        rule: &JwtIssuerRule,
        kid: &str,
    ) -> Result<Option<Jwk>, JwksUnavailable> {
        let entry = {
            let mut issuers = self.issuers.lock().expect("inbound JWKS lock poisoned");
            issuers
                .entry((rule.issuer.clone(), rule.jwks_url.clone()))
                .or_default()
                .clone()
        };
        // Fast path: a fresh set that holds `kid` answers without the fetch
        // lock, so it never waits behind a fetch another request started.
        let snapshot = entry.snapshot();
        if let Some(jwk) = self.fresh_hit(snapshot.as_ref(), kid) {
            return Ok(Some(jwk));
        }

        let mut state = entry.fetch.lock().await;
        // Another request may have refreshed while this one waited.
        let snapshot = entry.snapshot();
        if let Some(jwk) = self.fresh_hit(snapshot.as_ref(), kid) {
            return Ok(Some(jwk));
        }
        let stale = snapshot
            .as_ref()
            .is_some_and(|snap| snap.fetched_at.elapsed() >= self.max_age);

        let due = state
            .last_fetch
            .is_none_or(|at| at.elapsed() >= self.min_refetch);
        if !due {
            // Inside the refetch floor. With keys cached this is simply an
            // unknown `kid` (or a stale set whose refresh just failed, which
            // keeps serving); with none cached shunt still cannot verify
            // anything for this issuer, and answering `401` would misreport a
            // continuing outage as a bad credential.
            return match &snapshot {
                Some(snap) => Ok(snap.keys.find(kid).cloned()),
                None => Err(JwksUnavailable),
            };
        }
        state.last_fetch = Some(Instant::now());

        // A discovered `jwks_uri` is re-resolved when the set expires, so a
        // URI the issuer moved is picked up; a configured URL never changes.
        // Discovery is also redone while no set has been fetched yet, so a
        // document that once advertised a broken endpoint does not pin it.
        // When re-discovery fails, the URI the cached set came from is still
        // the best guess: a discovery outage must not also block key rotation
        // on a JWKS endpoint that is still up.
        let fetched = match (&rule.jwks_url, &state.jwks_url) {
            (Some(url), _) => Ok(url.clone()),
            (None, Some(url)) if !stale && snapshot.is_some() => Ok(url.clone()),
            (None, previous) => match self.resolve_jwks_url(rule).await {
                Ok(url) => Ok(url),
                Err(JwksUnavailable) => previous.clone().ok_or(JwksUnavailable),
            },
        };
        let fetched = match fetched {
            Ok(url) => match self.fetch_jwks(&url).await {
                Ok(keys) => Ok((url, keys)),
                Err(error) => {
                    tracing::warn!(
                        issuer = %rule.issuer,
                        error = %error,
                        "inbound JWT: JWKS fetch failed"
                    );
                    Err(())
                }
            },
            Err(JwksUnavailable) => Err(()),
        };
        match fetched {
            Ok((url, keys)) => {
                // Replace, never merge: a kid the issuer dropped must stop
                // verifying.
                if rule.jwks_url.is_none() {
                    state.jwks_url = Some(url);
                }
                let keys = Arc::new(keys);
                *entry
                    .snapshot
                    .lock()
                    .expect("inbound JWKS snapshot lock poisoned") = Some(KeySnapshot {
                    keys: keys.clone(),
                    fetched_at: Instant::now(),
                });
                Ok(keys.find(kid).cloned())
            }
            // A previously-fetched key set is still the best available
            // answer, even past its max age: an IdP outage must not become a
            // total outage. Only a cold cache is an outage from the caller's
            // point of view.
            Err(()) => match &snapshot {
                Some(snap) => {
                    if stale {
                        tracing::warn!(
                            issuer = %rule.issuer,
                            "inbound JWT: key set refresh failed; serving the expired key set"
                        );
                    }
                    Ok(snap.keys.find(kid).cloned())
                }
                None => Err(JwksUnavailable),
            },
        }
    }

    /// `kid` in a key set younger than the max age, if the snapshot has one.
    fn fresh_hit(&self, snapshot: Option<&KeySnapshot>, kid: &str) -> Option<Jwk> {
        let snap = snapshot.filter(|snap| snap.fetched_at.elapsed() < self.max_age)?;
        snap.keys.find(kid).cloned()
    }

    /// The configured `jwks_url`, or the `jwks_uri` from the issuer's discovery
    /// document. Both go through [`validate_endpoint`].
    async fn resolve_jwks_url(&self, rule: &JwtIssuerRule) -> Result<String, JwksUnavailable> {
        if let Some(url) = &rule.jwks_url {
            return Ok(url.clone());
        }
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            rule.issuer.trim_end_matches('/')
        );
        let document: DiscoveryDocument =
            self.fetch_json(&discovery_url).await.map_err(|error| {
                tracing::warn!(
                    issuer = %rule.issuer,
                    error = %error,
                    "inbound JWT: OIDC discovery failed"
                );
                JwksUnavailable
            })?;
        if document.issuer.trim_end_matches('/') != rule.issuer.trim_end_matches('/') {
            tracing::warn!(
                issuer = %rule.issuer,
                "inbound JWT: discovery document issuer does not match the configured issuer"
            );
            return Err(JwksUnavailable);
        }
        validate_endpoint(&document.jwks_uri).map_err(|message| {
            tracing::warn!(issuer = %rule.issuer, reason = %message, "inbound JWT: discovered jwks_uri rejected");
            JwksUnavailable
        })?;
        Ok(document.jwks_uri)
    }

    async fn fetch_jwks(&self, url: &str) -> Result<JwkSet, String> {
        self.fetch_json(url).await
    }

    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let response = self
            .client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            // Drain (bounded) so the connection returns to the pool; the status
            // is the error worth reporting, not whatever the body held.
            let _ = read_bounded(response).await;
            return Err(format!("returned HTTP {status}"));
        }
        let body = read_bounded(response).await?;
        serde_json::from_slice(&body).map_err(|error| format!("invalid JSON: {error}"))
    }
}

/// Only the two fields the JWKS path needs. `crate::gateway::idp_client`'s
/// `DiscoveredEndpoints` requires the authorization/token/userinfo endpoints,
/// which a verify-only deployment's issuer has no reason to serve.
#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

/// Read at most [`MAX_DOCUMENT_BYTES`], failing rather than truncating: a
/// truncated JWKS would parse as "this issuer has fewer keys than it does" and
/// silently reject tokens signed with the ones that were cut off.
async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("response stream failed: {error}"))?;
        if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(format!("response exceeds {MAX_DOCUMENT_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
