//! Antigravity subscription OAuth credential store.
//!
//! Antigravity reaches the same Code Assist backend the `gemini` provider uses
//! — `cloudcode-pa.googleapis.com/v1internal` by default, or whatever host the
//! provider's `base_url` configures (see [`AntigravityAuthStore::new`]) — but
//! with its own OAuth client and scopes, and it identifies itself as
//! `ideType: ANTIGRAVITY` during project discovery. The two credentials are
//! therefore not interchangeable, which is why this store exists alongside
//! [`crate::auth::google::auth`] rather than extending it.
//!
//! The credential file (`~/.shunt/antigravity-auth.json`, overridable via
//! `SHUNT_ANTIGRAVITY_AUTH_FILE`) is written by `shunt login antigravity` (see
//! [`super::login`]) and owned solely by shunt. Token values are never logged —
//! only refresh outcomes.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::adapters::AdapterError;
use crate::auth::auth_error;
use crate::auth::shared::write_auth_file_atomic;

/// Antigravity's OAuth client. Published as a plain constant in the Antigravity
/// client and mirrored by every third-party implementation of this flow.
///
/// The accompanying "secret" is not confidential and is not shunt's: per
/// RFC 8252 §8.5 a native application cannot keep a client secret, so Google
/// issues installed-app clients a value that is extractable from any shipped
/// copy of the binary. It is carried here for the same reason the client id is
/// — the token endpoint rejects the exchange without it. AGENTS.md's "never
/// commit secrets" rule governs shunt's own credentials; this is a third-party
/// public constant and is exempt by that reading. Do not obfuscate the literal
/// to dodge secret scanning: resolve the finding as a documented false positive
/// instead, so the next reader can still see what is being sent.
pub(crate) const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub(crate) const CLIENT_SECRET: &str = "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";

/// Fixed loopback callback port, matching the redirect URI registered for
/// [`CLIENT_ID`]. Unlike shunt's other loopback logins this cannot use an
/// ephemeral port.
pub(crate) const CALLBACK_PORT: u16 = 51121;
pub(crate) const CALLBACK_PATH: &str = "/oauth-callback";

pub(crate) const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";

/// Production Code Assist backend. This is only the default: `discover_project`
/// and inference (`adapters::gemini`) both address the provider's configured
/// `base_url`, which `AntigravityAuthStore::new` derives `api_endpoint` from —
/// see there for why. This constant remains the production value that
/// `main.rs`'s login command falls back to when no config is available.
pub(crate) const API_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
/// Onboarding is served from the `daily-` host, not the production one — but
/// only when the configured backend actually is production; see
/// `AntigravityAuthStore::new` for the non-production case.
pub(crate) const DAILY_API_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub(crate) const API_VERSION: &str = "v1internal";

/// Antigravity requests two scopes the Gemini CLI never asks for — `cclog` and
/// `experimentsandconfigs` — which is the concrete reason a Gemini CLI token
/// cannot be reused here even though both target the same backend.
pub(crate) const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Refresh this long before `expiry_date` rather than at it.
const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// Bound on a single onboarding request/response leg — the initial
/// `onboardUser` POST, or one operation-poll GET — split into its own send
/// and body-read window, so a single leg can block for up to twice this.
const ONBOARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait between operation polls, matching the interval the
/// reference client uses (`packages/core/src/code_assist/setup.ts` in
/// `google-gemini/gemini-cli`, Apache-2.0).
const ONBOARD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Wall-clock cap on how long `onboard_user` keeps polling the long-running
/// operation the initial `onboardUser` POST returns, once polling has
/// actually started — this does not bound the initial POST itself (see its
/// own two `ONBOARD_REQUEST_TIMEOUT` windows in `onboard_user` below).
///
/// The reference client polls forever (`while (!lroRes.done)`, no cap); that
/// is not safe to copy — a stuck operation would hang this call, and on the
/// request path `REFRESH_LOCK` with it, indefinitely. The scheme this
/// replaces bounded polling by attempt count instead of wall clock and
/// really only budgeted `4 * 2s` = 8s of waiting between its 5 attempts
/// (its documented 308s worst case was almost entirely per-attempt request
/// timeouts, not time spent actually waiting on the operation) — first-time
/// project provisioning is a background approval-and-resource-creation step
/// that routinely needs longer than that, which is why first-time onboarding
/// was failing. Five minutes gives real provisioning room to finish while
/// still failing a genuinely stuck operation well inside an interactive
/// login.
const ONBOARD_POLL_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Bound on a single request/response leg of the token-refresh and
/// project-discovery calls. Both are reached per-request from
/// `resolve_credential` (`src/auth/mod.rs`), ahead of the request-path
/// `upstream_timeout::wait` that only wraps the inference send — without this,
/// a stalled Google endpoint would hang every request indefinitely.
const CREDENTIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// In-process single-flight for the refresh path, mirroring the other stores.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityCred {
    pub access_token: String,
    pub project_id: String,
}

/// The on-disk credential shape. `project_id` is resolved once at login (where
/// the `onboardUser` poll can afford to block) and cached here so the request
/// path never pays for discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredAuth {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expiry_date: Option<u64>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AntigravityAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    token_url: String,
    api_endpoint: String,
    daily_api_endpoint: String,
    project_cache: Arc<RwLock<Option<String>>>,
    /// Bound on a single leg (send, or body read) of the refresh/discovery
    /// calls. Fixed at [`CREDENTIAL_REQUEST_TIMEOUT`] outside tests.
    request_timeout: Duration,
    /// How long `poll_onboard_operation` sleeps between polls. Fixed at
    /// [`ONBOARD_POLL_INTERVAL`] outside tests — a real Google endpoint needs
    /// that much headroom between polls, but a test against a local mock
    /// server does not, and `ONBOARD_POLL_INTERVAL` is 5s.
    onboard_poll_interval: Duration,
}

impl AntigravityAuthStore {
    /// `base_url` is the provider's configured Code Assist host — the same
    /// host `adapters::gemini` sends inference to (`provider.base_url` in
    /// `src/adapters/gemini/mod.rs`) — so that credential-path project
    /// discovery and onboarding address the backend the operator actually
    /// configured, rather than always reaching the hardcoded production host
    /// regardless of `base_url`. Trailing slashes are trimmed once here,
    /// mirroring the Gemini adapter's own normalization at the point it
    /// builds a request URL; no fallback to [`API_ENDPOINT`] is applied for an
    /// empty or malformed value — config validation already parses every
    /// provider `base_url` (`provider_base_url` in `src/config.rs`), and
    /// silently falling back here would fail open to the production host.
    pub fn new(path: PathBuf, client: reqwest::Client, base_url: impl Into<String>) -> Self {
        let api_endpoint = base_url.into().trim_end_matches('/').to_string();
        // The `daily-` control-plane host that serves onboarding only exists
        // for the production backend. Anything else config validation admits
        // (the `AuthMode::AntigravityOauth` block in `src/config.rs`) is a
        // loopback host — the operator's proxy standing in for the whole
        // backend — so sending `onboardUser` to the real `daily-` host there
        // would egress straight past the very endpoint the operator
        // configured. `discover_project` chains into `onboard_user` on the same
        // request path, so this has to travel with `api_endpoint` rather than
        // stay pinned to the production default.
        let daily_api_endpoint = if addresses_production_backend(&api_endpoint) {
            DAILY_API_ENDPOINT.to_string()
        } else {
            api_endpoint.clone()
        };
        Self {
            path,
            client,
            token_url: TOKEN_URL.to_string(),
            api_endpoint,
            daily_api_endpoint,
            project_cache: Arc::new(RwLock::new(None)),
            request_timeout: CREDENTIAL_REQUEST_TIMEOUT,
            onboard_poll_interval: ONBOARD_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_urls(
        path: PathBuf,
        client: reqwest::Client,
        token_url: String,
        api_endpoint: String,
        daily_api_endpoint: String,
    ) -> Self {
        Self {
            path,
            client,
            token_url,
            api_endpoint,
            daily_api_endpoint,
            project_cache: Arc::new(RwLock::new(None)),
            request_timeout: CREDENTIAL_REQUEST_TIMEOUT,
            onboard_poll_interval: ONBOARD_POLL_INTERVAL,
        }
    }

    /// Test-only override so a stalled-endpoint test does not have to wait out
    /// the real [`CREDENTIAL_REQUEST_TIMEOUT`].
    #[cfg(test)]
    pub(crate) fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Test-only override so an onboarding-poll test does not have to wait
    /// out the real 5s [`ONBOARD_POLL_INTERVAL`] between polls.
    #[cfg(test)]
    pub(crate) fn with_onboard_poll_interval(mut self, interval: Duration) -> Self {
        self.onboard_poll_interval = interval;
        self
    }

    /// Return a valid access token plus the Code Assist project id, refreshing
    /// the token when it is stale.
    pub async fn get_valid(&self) -> Result<AntigravityCred, AdapterError> {
        let stored = self.read().await?;
        if is_stored_valid(&stored, SystemTime::now()) {
            let project_id = self.project_id(&stored).await?;
            return Ok(AntigravityCred {
                access_token: stored.access_token,
                project_id,
            });
        }

        let _guard = REFRESH_LOCK.lock().await;
        // Everything below, up to and including the `project_id` call after
        // `write`, runs while holding this process-global lock — so every
        // other concurrent Antigravity request is blocked on it too, not just
        // the token refresh. `project_id` can chain through `discover_project`
        // into `onboard_user` when the stored credential has no project id
        // yet: refresh_call (send + body read, each up to
        // CREDENTIAL_REQUEST_TIMEOUT) + discover_project's loadCodeAssist
        // (send + body read, each up to CREDENTIAL_REQUEST_TIMEOUT) +
        // onboard_user (its own POST: send + body read, each up to
        // ONBOARD_REQUEST_TIMEOUT, then up to ONBOARD_POLL_DEADLINE of
        // polling if the account isn't onboarded yet) —
        // 2*30s + 2*30s + (2*30s + 300s) = 60 + 60 + 360 = 480s, roughly
        // 8 minutes, worst case before this lock is released. See the
        // `onboard_user` timing note in `login.rs` for the derivation of the
        // 360s figure.
        // Re-read: another task may have refreshed while we waited on the lock.
        let stored = self.read().await?;
        if is_stored_valid(&stored, SystemTime::now()) {
            let project_id = self.project_id(&stored).await?;
            return Ok(AntigravityCred {
                access_token: stored.access_token,
                project_id,
            });
        }

        let refreshed = self.refresh_call(&stored.refresh_token).await?;
        let expires_in = refreshed.expires_in.unwrap_or(3600);
        let expiry_date = SystemTime::now()
            .checked_add(Duration::from_secs(expires_in))
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as u64);
        let updated = StoredAuth {
            access_token: refreshed.access_token,
            // Google does not rotate the refresh token on every exchange; keep
            // the existing one when the response omits it, or the next refresh
            // would have nothing to present.
            refresh_token: refreshed
                .refresh_token
                .unwrap_or_else(|| stored.refresh_token.clone()),
            expiry_date,
            email: stored.email.clone(),
            project_id: stored.project_id.clone(),
        };
        self.write(&updated).await?;

        let project_id = self.project_id(&updated).await?;
        Ok(AntigravityCred {
            access_token: updated.access_token,
            project_id,
        })
    }

    async fn read(&self) -> Result<StoredAuth, AdapterError> {
        let path = self.path.clone();
        let content = tokio::task::spawn_blocking(move || fs::read_to_string(&path))
            .await
            .map_err(|_| auth_error("failed to read Antigravity credentials"))?
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    auth_error(format!(
                        "Antigravity credential file not found at {}. Run `shunt login antigravity` to authenticate.",
                        self.path.display()
                    ))
                } else {
                    auth_error(format!(
                        "failed to read Antigravity credential file {}: {error}",
                        self.path.display()
                    ))
                }
            })?;

        serde_json::from_str::<StoredAuth>(&content).map_err(|error| {
            auth_error(format!(
                "invalid JSON in Antigravity credential file {}: {error}",
                self.path.display()
            ))
        })
    }

    async fn write(&self, stored: &StoredAuth) -> Result<(), AdapterError> {
        let path = self.path.clone();
        let value = serde_json::to_value(stored).map_err(|error| {
            auth_error(format!("failed to encode Antigravity credentials: {error}"))
        })?;
        tokio::task::spawn_blocking(move || write_auth_file_atomic(&path, &value))
            .await
            .map_err(|_| auth_error("Antigravity credential write task failed"))?
            .map_err(|error| {
                auth_error(format!(
                    "failed to write Antigravity credential file {}: {error}",
                    self.path.display()
                ))
            })
    }

    async fn refresh_call(&self, refresh_token: &str) -> Result<TokenResponse, AdapterError> {
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("refresh_token", refresh_token),
        ];
        // `self.client` follows redirects freely; this POST carries the
        // long-lived refresh_token, so it goes through the redirect-hardened
        // `token_refresh_client()` instead — a permitted token endpoint must
        // not be able to 3xx the credential to a plaintext/off-loopback host.
        let response = tokio::time::timeout(
            self.request_timeout,
            crate::auth::shared::token_refresh_client()
                .post(&self.token_url)
                .form(&params)
                .send(),
        )
        .await
        .map_err(|_| auth_error("Antigravity token refresh timed out"))?
        .map_err(|error| {
            auth_error(format!(
                "Antigravity token refresh network failure: {error}"
            ))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = diagnostic_body(self.request_timeout, response).await;
            tracing::warn!(
                status = %status,
                body = %body,
                "Antigravity OAuth token refresh rejected"
            );
            return Err(auth_error(
                "Antigravity OAuth token expired or revoked. Run `shunt login antigravity` to re-authenticate.",
            ));
        }

        tokio::time::timeout(self.request_timeout, response.json::<TokenResponse>())
            .await
            .map_err(|_| auth_error("Antigravity token refresh timed out reading the response"))?
            .map_err(|error| {
                auth_error(format!(
                    "invalid JSON response from Google token endpoint: {error}"
                ))
            })
    }

    /// The Code Assist project id: the persisted one when login resolved it,
    /// otherwise a discovery round trip cached for the lifetime of this store.
    /// `resolve_credential` constructs a fresh store per request, so this cache
    /// does not survive past the request that populated it — what actually
    /// persists across requests is the `project_id` [`write_stored`] writes into
    /// the on-disk credential file once discovery succeeds.
    async fn project_id(&self, stored: &StoredAuth) -> Result<String, AdapterError> {
        if let Some(project) = stored.project_id.as_deref().filter(|id| !id.is_empty()) {
            return Ok(project.to_string());
        }
        {
            let cache = self.project_cache.read().await;
            if let Some(project) = cache.as_ref() {
                return Ok(project.clone());
            }
        }
        let project = self
            .discover_project(&stored.access_token)
            .await
            .inspect_err(|error| {
                // Discovery on the request path means login did not persist a
                // project id; say so rather than surfacing a bare HTTP failure.
                tracing::warn!("Antigravity project discovery failed: {}", error.message);
            })?;
        let mut cache = self.project_cache.write().await;
        *cache = Some(project.clone());
        drop(cache);

        // Persist the discovered id to disk (preserving every other field), not
        // just the in-memory cache: `resolve_credential` builds a fresh store
        // per request, so without this every request from a project-less
        // credential would keep re-discovering until a login happened to write
        // one. Best-effort — a write failure here must not fail a request that
        // already has a working access token and project id in hand.
        //
        // `stored` is a snapshot this method's caller took before
        // `discover_project` ran — and that call can take minutes on
        // first-time onboarding (`ONBOARD_POLL_DEADLINE`). Writing `stored`
        // back verbatim (plus `project_id`) would clobber a token pair a
        // concurrent request refreshed in that window, silently discarding a
        // successful refresh and costing the operator a full re-login for a
        // lost rotated refresh token. So re-read the file immediately before
        // writing and merge only `project_id` into *that* record, never the
        // stale `stored` snapshot's token fields.
        //
        // Re-reading is necessary but not sufficient: the record on disk may
        // no longer be the one discovery ran against at all. The guards below
        // narrow the write to the case where it still is, and where nobody
        // else has already resolved a project id.
        //
        // This re-read/write pair does not touch `REFRESH_LOCK` — `read` and
        // `write` are plain file I/O with no locking of their own — so it is
        // safe to call `project_id` regardless of whether the caller already
        // holds that lock (it does on two of its three call sites in
        // `get_valid`, but not on the fast, already-valid path).
        match self.read().await {
            Ok(mut fresh) => {
                // A project id belongs to the identity discovery ran against,
                // so merging it into whatever the file holds now is only
                // correct while that is still the same account. `shunt login
                // antigravity` replaces the file wholesale, so a login landing
                // inside the discovery window would otherwise pair the new
                // account's access token with a project provisioned for the
                // old one.
                let same_identity = match (stored.email.as_deref(), fresh.email.as_deref()) {
                    (Some(before), Some(after)) => before == after,
                    // No pair of emails to compare — fall back to the refresh
                    // token. A login always mints a new one, so an unchanged
                    // value means this is still the record discovery ran
                    // against. A changed one is ambiguous (a concurrent
                    // refresh rotates it too), and skipping costs only a
                    // rediscovery on the next request.
                    _ => fresh.refresh_token == stored.refresh_token,
                };
                let already_resolved = fresh.project_id.as_deref().is_some_and(|id| !id.is_empty());

                if !same_identity {
                    tracing::warn!(
                        "skipped persisting discovered Antigravity project id: \
                         the credential file no longer holds the account it was discovered for"
                    );
                } else if already_resolved {
                    // A login or a concurrent discovery resolved one while
                    // this call was in flight. That value is at least as fresh
                    // as ours, so leave it alone.
                    tracing::debug!(
                        "Antigravity project id already persisted by a concurrent writer"
                    );
                } else {
                    fresh.project_id = Some(project.clone());
                    if let Err(error) = self.write(&fresh).await {
                        tracing::warn!(
                            "failed to persist discovered Antigravity project id: {}",
                            error.message
                        );
                    }
                }
            }
            Err(error) => {
                // The on-disk record disappeared or became unreadable between
                // the stale snapshot and this re-read. Do not resurrect it
                // from `stored` — that could restore a credential a
                // concurrent login or refresh has already superseded. Skip
                // the persist; the caller still gets a working access token
                // and project id in hand, and the next request will simply
                // rediscover.
                tracing::warn!(
                    "skipped persisting discovered Antigravity project id: {}",
                    error.message
                );
            }
        }

        Ok(project)
    }

    /// `loadCodeAssist`, falling back to `onboardUser` when the account has no
    /// project provisioned yet.
    pub(crate) async fn discover_project(
        &self,
        access_token: &str,
    ) -> Result<String, AdapterError> {
        let url = format!("{}/{}:loadCodeAssist", self.api_endpoint, API_VERSION);
        let response = tokio::time::timeout(
            self.request_timeout,
            self.client
                .post(&url)
                .bearer_auth(access_token)
                .header("User-Agent", super::version::user_agent())
                .json(&json!({ "metadata": load_code_assist_metadata() }))
                .send(),
        )
        .await
        .map_err(|_| auth_error("Antigravity project discovery timed out"))?
        .map_err(|error| {
            auth_error(format!(
                "Antigravity project discovery network failure: {error}"
            ))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = diagnostic_body(self.request_timeout, response).await;
            tracing::warn!(
                status = %status,
                body = %body,
                "Antigravity loadCodeAssist rejected"
            );
            return Err(auth_error(format!(
                "Antigravity project discovery failed with HTTP status {status}"
            )));
        }

        let body = tokio::time::timeout(self.request_timeout, response.json::<Value>())
            .await
            .map_err(|_| {
                auth_error("Antigravity project discovery timed out reading the response")
            })?
            .map_err(|error| auth_error(format!("invalid JSON from loadCodeAssist: {error}")))?;

        if let Some(project) = extract_project(&body) {
            return Ok(project);
        }
        self.onboard_user(access_token, &default_tier_id(&body))
            .await
    }

    /// Provision a project for a first-time account. The control plane
    /// answers the initial `onboardUser` POST with a
    /// `google.longrunning.Operation` (`google/longrunning/operations.proto`):
    /// if it is not already `done`, that response carries a `name` to poll
    /// rather than a result to act on. Poll
    /// `GET {daily_api_endpoint}/{API_VERSION}/{name}` — with the same bearer
    /// token and identity headers as the POST — until `done`; never re-POST
    /// `onboardUser` itself on a retry, which the operation contract does not
    /// call for.
    async fn onboard_user(
        &self,
        access_token: &str,
        tier_id: &str,
    ) -> Result<String, AdapterError> {
        let url = format!("{}/{}:onboardUser", self.daily_api_endpoint, API_VERSION);
        // Onboarding is served by the control plane, which the Antigravity
        // client reaches through its Node google-api client rather than the
        // Hub agent used for inference — so the identity here differs from
        // `loadCodeAssist` above.
        let user_agent = super::version::node_user_agent();
        let body = json!({
            "tier_id": tier_id,
            "metadata": control_plane_metadata(),
        });

        let response = tokio::time::timeout(
            ONBOARD_REQUEST_TIMEOUT,
            self.client
                .post(&url)
                .bearer_auth(access_token)
                .header("User-Agent", &user_agent)
                .header("X-Goog-Api-Client", super::version::GOOG_API_CLIENT)
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| auth_error("Antigravity onboardUser timed out"))?
        .map_err(|error| auth_error(format!("Antigravity onboardUser failed: {error}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = diagnostic_body(ONBOARD_REQUEST_TIMEOUT, response).await;
            tracing::warn!(
                status = %status,
                body = %body,
                "Antigravity onboardUser rejected"
            );
            return Err(auth_error(format!(
                "Antigravity account onboarding failed with HTTP status {status}"
            )));
        }

        let payload = tokio::time::timeout(ONBOARD_REQUEST_TIMEOUT, response.json::<Value>())
            .await
            .map_err(|_| auth_error("Antigravity onboardUser timed out reading the response"))?
            .map_err(|error| auth_error(format!("invalid JSON from onboardUser: {error}")))?;

        match onboard_operation_outcome(&payload)? {
            OnboardOperation::Done(project) => Ok(project),
            OnboardOperation::Pending(name) => tokio::time::timeout(
                ONBOARD_POLL_DEADLINE,
                self.poll_onboard_operation(access_token, &user_agent, &name),
            )
            .await
            .map_err(|_| {
                auth_error(format!(
                    "Antigravity account onboarding did not complete within {ONBOARD_POLL_DEADLINE:?} of polling"
                ))
            })?,
        }
    }

    /// Poll the named long-running operation `onboard_user`'s initial POST
    /// returned, until it reports `done`, sleeping [`ONBOARD_POLL_INTERVAL`]
    /// before each poll — matching the reference client's cadence
    /// (`packages/core/src/code_assist/setup.ts` in `google-gemini/gemini-cli`,
    /// Apache-2.0). This loop has no cap of its own — unlike the reference,
    /// which polls forever — because the caller wraps it in
    /// [`ONBOARD_POLL_DEADLINE`].
    async fn poll_onboard_operation(
        &self,
        access_token: &str,
        user_agent: &str,
        name: &str,
    ) -> Result<String, AdapterError> {
        let url = format!("{}/{}/{}", self.daily_api_endpoint, API_VERSION, name);
        loop {
            tokio::time::sleep(self.onboard_poll_interval).await;

            let response = tokio::time::timeout(
                ONBOARD_REQUEST_TIMEOUT,
                self.client
                    .get(&url)
                    .bearer_auth(access_token)
                    .header("User-Agent", user_agent)
                    .header("X-Goog-Api-Client", super::version::GOOG_API_CLIENT)
                    .send(),
            )
            .await
            .map_err(|_| auth_error("Antigravity onboarding operation poll timed out"))?
            .map_err(|error| {
                auth_error(format!(
                    "Antigravity onboarding operation poll failed: {error}"
                ))
            })?;

            if !response.status().is_success() {
                let status = response.status();
                let body = diagnostic_body(ONBOARD_REQUEST_TIMEOUT, response).await;
                tracing::warn!(
                    status = %status,
                    body = %body,
                    "Antigravity onboarding operation poll rejected"
                );
                return Err(auth_error(format!(
                    "Antigravity onboarding operation poll failed with HTTP status {status}"
                )));
            }

            let payload = tokio::time::timeout(ONBOARD_REQUEST_TIMEOUT, response.json::<Value>())
                .await
                .map_err(|_| {
                    auth_error(
                        "Antigravity onboarding operation poll timed out reading the response",
                    )
                })?
                .map_err(|error| {
                    auth_error(format!(
                        "invalid JSON from the onboarding operation poll: {error}"
                    ))
                })?;

            if let OnboardOperation::Done(project) = onboard_operation_outcome(&payload)? {
                return Ok(project);
            }
            // Still pending: keep polling the same `name` this loop was
            // handed — a payload's own `name` echo, if present, never
            // changes mid-operation.
        }
    }
}

/// Identifies the caller to project discovery. Sending the Gemini CLI's
/// identity here would provision the wrong kind of project.
pub(crate) fn load_code_assist_metadata() -> Value {
    json!({ "ideType": "ANTIGRAVITY" })
}

pub(crate) fn control_plane_metadata() -> Value {
    json!({
        "ide_type": "ANTIGRAVITY",
        "ide_name": "antigravity",
        "ide_version": super::version::current(),
    })
}

/// Upper bound on how much of a non-2xx body [`diagnostic_body`] reads and
/// logs. This is a diagnostic aid, not the response the caller acts on — a
/// hostile or merely oversized upstream must not be able to make shunt buffer
/// an unbounded body into memory and then write all of it into a `tracing`
/// field. A few KiB is comfortably enough to eyeball the shape of an error
/// response (JSON error bodies from these endpoints are far smaller).
const DIAGNOSTIC_BODY_MAX_BYTES: usize = 4096;

/// Best-effort diagnostic read of a non-2xx response body for the `tracing::warn!`
/// alongside it. An empty body, a read that errors mid-stream, and a read that
/// times out are three different failure shapes; collapsing them all into `""`
/// would make the log line as ambiguous as the bare status code it accompanies.
/// This is diagnostic-only — the caller still receives a distinct [`AdapterError`]
/// regardless of which of these three a rejected response hits.
///
/// The read is capped at [`DIAGNOSTIC_BODY_MAX_BYTES`]: rather than draining
/// the body in full with [`reqwest::Response::text`] and only truncating the
/// string afterwards (which still buffers the whole thing), this reads chunk
/// by chunk and folds each one through [`accumulate_diagnostic_chunk`], which
/// keeps `buf` at or under the cap no matter how large an individual chunk
/// is. The loop itself keeps polling `response.chunk()` until the body ends
/// (`Ok(None)`) or errors, even after the cap is hit -- stopping early there
/// would leave the rest of the body unread and strand the reqwest connection
/// instead of returning it to the pool, the same problem this function was
/// introduced to fix for its callers' non-2xx responses in the first place.
/// A body cut short this way is marked in the returned string so the
/// truncation itself is visible in the log line rather than looking like the
/// response actually ended there.
///
/// `pub(super)` rather than private: `version.rs` and `login.rs` (siblings under
/// `antigravity`) reuse this to drain their own non-2xx responses rather than
/// dropping them un-drained, which would otherwise strand the reqwest
/// connection instead of returning it to the pool.
pub(super) async fn diagnostic_body(timeout: Duration, mut response: reqwest::Response) -> String {
    let read = async {
        let mut buf = Vec::new();
        let mut truncated = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => accumulate_diagnostic_chunk(&mut buf, &chunk, &mut truncated),
                Ok(None) => break,
                Err(error) => return Err(format!("<error reading body: {error}>")),
            }
        }
        Ok((buf, truncated))
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(Ok((buf, _))) if buf.is_empty() => "<empty body>".to_string(),
        Ok(Ok((buf, truncated))) => {
            let mut text = String::from_utf8_lossy(&buf).into_owned();
            if truncated {
                text.push_str(&format!(
                    "...<truncated after {DIAGNOSTIC_BODY_MAX_BYTES} bytes>"
                ));
            }
            text
        }
        Ok(Err(message)) => message,
        Err(_) => "<timed out reading body>".to_string(),
    }
}

/// Fold one body chunk into the bounded diagnostic buffer, keeping `buf` at
/// or under [`DIAGNOSTIC_BODY_MAX_BYTES`] regardless of how large `chunk`
/// itself is. The previous version only checked the cap *between* chunks
/// (`if buf.len() >= CAP { break }` before each read), so a single chunk
/// larger than the whole cap -- an entirely ordinary shape for a small
/// upstream error body, since `reqwest::Response::chunk` yields whatever the
/// transport handed it, not a fixed size -- sailed straight through and blew
/// past the bound in one step. Splitting an oversized chunk at the remaining
/// budget instead means the cap holds no matter how the body happens to be
/// chunked on the wire.
///
/// `truncated` is only ever set, never cleared: once part of the body has
/// been dropped, a later empty or already-capped chunk must not make the
/// result look complete again.
fn accumulate_diagnostic_chunk(buf: &mut Vec<u8>, chunk: &[u8], truncated: &mut bool) {
    let remaining = DIAGNOSTIC_BODY_MAX_BYTES.saturating_sub(buf.len());
    if chunk.len() <= remaining {
        buf.extend_from_slice(chunk);
    } else {
        buf.extend_from_slice(&chunk[..remaining]);
        *truncated = true;
    }
}

/// Outcome of interpreting one `onboardUser`/operation-poll JSON payload
/// against the `google.longrunning.Operation` contract: a `done` field, and —
/// only once `done` is true — mutually exclusive `response` or `error`
/// fields (`oneof result` in `google/longrunning/operations.proto`).
enum OnboardOperation {
    /// `done: true`, with a project id extracted from `response`.
    Done(String),
    /// Not yet `done`; carries the operation `name` to poll next.
    Pending(String),
}

/// Interpret a single onboarding JSON payload — the initial `onboardUser`
/// response, or a later poll of the operation it returned — against the
/// operation contract [`OnboardOperation`] documents. A `done: true` payload
/// carrying `error` is a server-side onboarding failure and is reported as
/// such, rather than falling through to "completed without a project id",
/// which would misreport it as a merely-missing field.
fn onboard_operation_outcome(payload: &Value) -> Result<OnboardOperation, AdapterError> {
    if payload.get("done").and_then(Value::as_bool) != Some(true) {
        return match payload
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => Ok(OnboardOperation::Pending(name.to_string())),
            // Nothing to poll and nothing done: a protocol violation, not
            // something to retry into.
            None => Err(auth_error(
                "Antigravity onboarding returned an incomplete operation with no name to poll",
            )),
        };
    }

    if let Some(error) = payload.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        return Err(auth_error(match code {
            Some(code) => {
                format!("Antigravity account onboarding failed: operation error {code}: {message}")
            }
            None => format!("Antigravity account onboarding failed: operation error: {message}"),
        }));
    }

    payload
        .get("response")
        .and_then(extract_project)
        .map(OnboardOperation::Done)
        .ok_or_else(|| auth_error("Antigravity onboardUser completed without a project id"))
}

/// Pull the project id out of a `loadCodeAssist` / `onboardUser` payload. The
/// field has four spellings across the two endpoints, and `project` may be an
/// object rather than a string. `companionProject` is the same spelling the
/// Gemini Code Assist credential path accepts (`google::auth`'s
/// `cloudaicompanion_project` alias) — the two talk to the same backend, so
/// missing it here would send an already-provisioned account down the
/// `onboardUser` path instead of using the project it just returned.
pub(crate) fn extract_project(value: &Value) -> Option<String> {
    for key in [
        "cloudaicompanionProject",
        "companionProject",
        "projectId",
        "project",
    ] {
        match value.get(key) {
            Some(Value::String(id)) => {
                let trimmed = id.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            Some(Value::Object(map)) => {
                if let Some(Value::String(id)) = map.get("id") {
                    let trimmed = id.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// The tier to onboard into: the one flagged `isDefault`, else whatever the
/// account currently sits on, else the free tier.
pub(crate) fn default_tier_id(load_response: &Value) -> String {
    if let Some(tiers) = load_response.get("allowedTiers").and_then(Value::as_array) {
        for tier in tiers {
            if tier.get("isDefault").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if let Some(id) = tier.get("id").and_then(Value::as_str) {
                let trimmed = id.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    if let Some(id) = load_response
        .get("currentTier")
        .and_then(|tier| tier.get("id"))
        .and_then(Value::as_str)
    {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "free-tier".to_string()
}

/// Whether `endpoint` addresses the production Code Assist backend itself,
/// which is what decides between the `daily-` control-plane host and the
/// configured host for onboarding (see [`AntigravityAuthStore::new`]).
///
/// Deliberately not `endpoint == API_ENDPOINT`. Config validation accepts any
/// `base_url` whose *parsed* host is the Code Assist host over https, and the
/// operator's raw spelling is what reaches here — so `https://CloudCode-PA.googleapis.com`
/// (the URL parser lowercases hosts) and `https://cloudcode-pa.googleapis.com:443`
/// (an explicit default port) are both the production backend while being
/// byte-different from the constant. A string compare sends their first-time
/// onboarding to the production host, which does not serve `onboardUser`.
/// Compare the parsed URL instead, reusing config's own host predicate, and
/// treat a port, a path prefix, or a non-https scheme as "not plain
/// production" — those address something in front of the backend, which is
/// exactly the case that must carry onboarding with it.
fn addresses_production_backend(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint).is_ok_and(|url| {
        url.scheme() == "https"
            && url.port().is_none()
            && url.path() == "/"
            && url
                .host_str()
                .is_some_and(crate::config::host_is_google_codeassist)
    })
}

/// Google issues opaque access tokens, so validity comes from the recorded
/// `expiry_date` alone. A credential without one is treated as stale rather
/// than assumed live — the cost is one refresh, and the alternative is sending
/// an expired bearer upstream.
fn is_stored_valid(stored: &StoredAuth, now: SystemTime) -> bool {
    if stored.access_token.is_empty() {
        return false;
    }
    let Some(expiry_ms) = stored.expiry_date else {
        return false;
    };
    // A corrupted or maliciously large `expiry_date` must not panic: `+` on
    // `SystemTime` panics on overflow, so use the checked form and treat an
    // out-of-range value as stale (fail-closed) rather than crash.
    let Some(expiry) = UNIX_EPOCH.checked_add(Duration::from_millis(expiry_ms)) else {
        return false;
    };
    expiry
        .checked_sub(EXPIRY_BUFFER)
        .is_some_and(|refresh_at| now < refresh_at)
}

/// Write a freshly minted credential. Creates the parent directory; the atomic
/// writer itself deliberately refuses to.
pub(crate) fn write_stored(path: &Path, stored: &StoredAuth) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let value = serde_json::to_value(stored)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_auth_file_atomic(path, &value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json_string, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn temp_auth_file(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("shunt_tests_agy_{name}_{id}"));
        let _ = fs::create_dir_all(&dir);
        dir.join("antigravity-auth.json")
    }

    fn store_at(path: PathBuf, server: &MockServer) -> AntigravityAuthStore {
        AntigravityAuthStore::with_urls(
            path,
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            server.uri(),
            server.uri(),
        )
    }

    fn future_millis(secs: u64) -> u64 {
        (SystemTime::now() + Duration::from_secs(secs))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn write(path: &Path, stored: &StoredAuth) {
        write_stored(path, stored).unwrap();
    }

    // `AntigravityAuthStore::new` derives both endpoints from the base_url its
    // caller passes it (issue #380: it used to hardcode the production host
    // regardless of what the provider was configured with). These four tests
    // exercise `new(...)` directly, unlike the rest of this module which goes
    // through `with_urls`/`store_at`, since `with_urls` bypasses `new` entirely
    // and would not catch a regression there.

    #[tokio::test]
    async fn discovery_addresses_the_configured_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "cloudaicompanionProject": "proj-configured"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = AntigravityAuthStore::new(
            temp_auth_file("configured_base_url"),
            reqwest::Client::new(),
            server.uri(),
        );
        let project = store.discover_project("access-token").await.unwrap();
        assert_eq!(project, "proj-configured");
        server.verify().await;
    }

    #[tokio::test]
    async fn a_trailing_slash_on_the_configured_base_url_does_not_double_up() {
        // The Gemini adapter trims a trailing slash before building its own
        // request URL (`adapters::gemini::mod`); `new` must normalize the
        // same way, or a trailing-slash base_url would send
        // `.../` + `/v1internal:loadCodeAssist` (double slash), which
        // wiremock's exact `path` matcher below would refuse to serve.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "cloudaicompanionProject": "proj-trimmed"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = AntigravityAuthStore::new(
            temp_auth_file("trailing_slash"),
            reqwest::Client::new(),
            format!("{}/", server.uri()),
        );
        let project = store.discover_project("access-token").await.unwrap();
        assert_eq!(project, "proj-trimmed");
        server.verify().await;
    }

    #[tokio::test]
    async fn onboarding_follows_the_configured_base_url_when_it_is_not_the_production_default() {
        // `discover_project` chains into `onboard_user` on the same request
        // path, so a non-production base_url must carry onboarding along with
        // it rather than splitting back to the production `daily-` host.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "allowedTiers": [{"id": "standard-tier", "isDefault": true}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": true,
                "response": {"cloudaicompanionProject": "proj-onboarded-configured"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = AntigravityAuthStore::new(
            temp_auth_file("onboard_configured_base_url"),
            reqwest::Client::new(),
            server.uri(),
        );
        let project = store.discover_project("access-token").await.unwrap();
        assert_eq!(project, "proj-onboarded-configured");
        server.verify().await;
    }

    #[test]
    fn the_production_default_still_routes_onboarding_to_the_daily_host() {
        // When the configured base_url resolves to the production default,
        // onboarding must keep using the dedicated `daily-` control-plane
        // host — only a genuinely non-production base_url (necessarily
        // loopback, per the AntigravityOauth validation guard in
        // src/config.rs) collapses daily_api_endpoint onto api_endpoint.
        let store = AntigravityAuthStore::new(
            temp_auth_file("production_default_daily_host"),
            reqwest::Client::new(),
            API_ENDPOINT,
        );
        assert_eq!(store.api_endpoint, API_ENDPOINT);
        assert_eq!(store.daily_api_endpoint, DAILY_API_ENDPOINT);
    }

    #[test]
    fn every_config_valid_spelling_of_the_production_host_still_reaches_the_daily_host() {
        // The production check cannot be a string compare against
        // API_ENDPOINT. Config validation admits any base_url whose *parsed*
        // host is the Code Assist host over https, and the operator's raw
        // spelling is what reaches the store — so a mis-cased host or an
        // explicit `:443` is production while being byte-different from the
        // constant. Treating those as non-production sends a first-time
        // account's `onboardUser` to the production host, which does not serve
        // it: a config the validator calls plain production would break.
        for spelling in [
            API_ENDPOINT,
            "https://CloudCode-PA.googleapis.com",
            "https://cloudcode-pa.googleapis.com:443",
        ] {
            assert!(
                addresses_production_backend(spelling),
                "{spelling} is the production backend"
            );
        }
        // Anything standing in *front* of the backend — a proxy port, a path
        // prefix, plaintext, or a different host — has to carry onboarding
        // with it instead.
        for spelling in [
            "https://cloudcode-pa.googleapis.com:8443",
            "https://cloudcode-pa.googleapis.com/debug-proxy",
            "http://cloudcode-pa.googleapis.com",
            "http://127.0.0.1:8080",
            "https://daily-cloudcode-pa.googleapis.com",
            "not a url",
        ] {
            assert!(
                !addresses_production_backend(spelling),
                "{spelling} is not the plain production backend"
            );
        }
    }

    #[test]
    fn a_non_canonical_production_base_url_keeps_onboarding_on_the_daily_host() {
        // The store-level mirror of the case above: `:443` is the same backend
        // as the bare host, so it must not collapse daily_api_endpoint onto it.
        let store = AntigravityAuthStore::new(
            temp_auth_file("production_with_explicit_port"),
            reqwest::Client::new(),
            "https://cloudcode-pa.googleapis.com:443",
        );
        assert_eq!(store.daily_api_endpoint, DAILY_API_ENDPOINT);
    }

    #[test]
    fn a_credential_without_an_expiry_is_treated_as_stale() {
        // Google issues opaque access tokens, so there is no embedded `exp` to
        // fall back on. Assuming validity would send an expired bearer upstream.
        let stored = StoredAuth {
            access_token: "opaque-ya29-token".to_string(),
            refresh_token: "refresh".to_string(),
            expiry_date: None,
            email: None,
            project_id: None,
        };
        assert!(!is_stored_valid(&stored, SystemTime::now()));
    }

    #[test]
    fn expiry_inside_the_buffer_counts_as_stale() {
        let mut stored = StoredAuth {
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expiry_date: Some(future_millis(60)),
            email: None,
            project_id: None,
        };
        // 60s out is inside the 5-minute refresh buffer.
        assert!(!is_stored_valid(&stored, SystemTime::now()));
        stored.expiry_date = Some(future_millis(3600));
        assert!(is_stored_valid(&stored, SystemTime::now()));
    }

    #[test]
    fn an_empty_access_token_is_never_valid() {
        let stored = StoredAuth {
            access_token: String::new(),
            refresh_token: "refresh".to_string(),
            expiry_date: Some(future_millis(3600)),
            email: None,
            project_id: None,
        };
        assert!(!is_stored_valid(&stored, SystemTime::now()));
    }

    #[test]
    fn a_corrupted_huge_expiry_never_panics() {
        // `UNIX_EPOCH + Duration::from_millis(expiry_ms)` panics on overflow
        // for a sufficiently large millisecond value; a corrupted credential
        // file must not be able to crash the process over this, so the
        // checked form is used instead and any overflow fails closed
        // (treated as stale) rather than propagating.
        //
        // `u64::MAX` milliseconds happens to still be representable as a
        // `SystemTime` on 64-bit Unix (comfortably inside its `i64`-seconds
        // range), so it is not itself an overflowing value here — the
        // property this test actually pins down, and the one the fix exists
        // for, is that no expiry_date value can ever panic this function.
        let stored = StoredAuth {
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expiry_date: Some(u64::MAX),
            email: None,
            project_id: None,
        };
        let result = std::panic::catch_unwind(|| is_stored_valid(&stored, SystemTime::now()));
        assert!(
            result.is_ok(),
            "is_stored_valid must not panic on a corrupted huge expiry_date"
        );
        // Pin the concrete, currently-representable outcome so this stays
        // non-vacuous: on this platform the value is a legitimate (if
        // absurdly distant) future expiry, not an overflow.
        assert!(result.unwrap());
    }

    #[test]
    fn project_is_extracted_from_every_spelling() {
        assert_eq!(
            extract_project(&json!({"cloudaicompanionProject": "p-1"})).as_deref(),
            Some("p-1")
        );
        assert_eq!(
            extract_project(&json!({"projectId": "p-2"})).as_deref(),
            Some("p-2")
        );
        // The Gemini Code Assist credential path accepts this spelling too
        // (`google::auth`'s `cloudaicompanion_project` alias). Both talk to
        // the same backend, so it has to resolve here as well.
        assert_eq!(
            extract_project(&json!({"companionProject": "p-4"})).as_deref(),
            Some("p-4")
        );
        // `project` arrives as an object on the onboarding response.
        assert_eq!(
            extract_project(&json!({"project": {"id": "p-3"}})).as_deref(),
            Some("p-3")
        );
        assert_eq!(extract_project(&json!({})), None);
        // A blank value must not be mistaken for a project id.
        assert_eq!(
            extract_project(&json!({"cloudaicompanionProject": "  "})),
            None
        );
    }

    #[test]
    fn default_tier_prefers_the_flagged_default_then_current_then_free() {
        let with_default = json!({
            "allowedTiers": [
                {"id": "legacy-tier"},
                {"id": "standard-tier", "isDefault": true}
            ],
            "currentTier": {"id": "current-tier"}
        });
        assert_eq!(default_tier_id(&with_default), "standard-tier");

        // No tier is flagged default: fall back to the one in force.
        let current_only = json!({
            "allowedTiers": [{"id": "legacy-tier"}],
            "currentTier": {"id": "current-tier"}
        });
        assert_eq!(default_tier_id(&current_only), "current-tier");

        assert_eq!(default_tier_id(&json!({})), "free-tier");
    }

    #[tokio::test]
    async fn discovery_identifies_itself_as_antigravity() {
        // Sending the Gemini CLI's identity here would provision the wrong kind
        // of project, so the metadata is asserted on the wire rather than
        // trusted to the caller.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(body_json_string(
                serde_json::to_string(&json!({"metadata": {"ideType": "ANTIGRAVITY"}})).unwrap(),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-agy"})),
            )
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("ide_type"), &server);
        let project = store.discover_project("token").await.unwrap();
        assert_eq!(project, "proj-agy");
    }

    #[tokio::test]
    async fn a_stalled_discovery_endpoint_times_out_instead_of_hanging() {
        // A stuck Google endpoint must not hang every request indefinitely —
        // `discover_project` is reached per-request from `resolve_credential`,
        // ahead of the request-path upstream timeout that only wraps the
        // inference send. Use a request timeout far shorter than the delayed
        // response so the test itself stays fast.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-agy"}))
                    .set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("stalled_discovery"), &server)
            .with_request_timeout(Duration::from_millis(20));
        let error = store
            .discover_project("token")
            .await
            .expect_err("a stalled endpoint must time out rather than hang");
        use axum::body::to_bytes;
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("timed out"),
            "expected a timeout error, got: {body}"
        );
    }

    #[tokio::test]
    async fn a_stalled_refresh_endpoint_times_out_instead_of_hanging() {
        // `refresh_call` runs on every stale-token request — far more often than
        // `discover_project` — so a stuck token endpoint must not hang it either.
        // Mirrors `a_stalled_discovery_endpoint_times_out_instead_of_hanging`
        // above for the sibling call.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "new-token", "expires_in": 3600}))
                    .set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("stalled_refresh"), &server)
            .with_request_timeout(Duration::from_millis(20));
        let error = store
            .refresh_call("refresh-token")
            .await
            .expect_err("a stalled token endpoint must time out rather than hang");
        use axum::body::to_bytes;
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("timed out"),
            "expected a timeout error, got: {body}"
        );
    }

    #[tokio::test]
    async fn refresh_call_refuses_a_redirect_to_an_offhost_plaintext_target() {
        // The redirect-hardening guard lives in `auth::shared::token_refresh_client`
        // and is exercised directly in `codex/auth.rs`; this proves `refresh_call`
        // itself is actually wired through it, rather than `self.client` (which
        // follows redirects freely).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", "http://evil.example/token"),
            )
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("refresh_redirect"), &server);
        let error = store
            .refresh_call("refresh-token")
            .await
            .expect_err("a redirect to a plaintext off-host target must be refused");
        use axum::body::to_bytes;
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("network failure"),
            "expected the refused redirect to surface as a network failure, got: {body}"
        );
    }

    #[tokio::test]
    async fn a_companion_project_spelling_is_used_instead_of_onboarding() {
        // `companionProject` is one of the spellings the backend uses (the
        // Gemini Code Assist path accepts it too). Missing it here would read
        // an already-provisioned account as project-less and spend the
        // onboarding budget against the daily control plane instead of using
        // the project `loadCodeAssist` just handed back.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"companionProject": "proj-companion"})),
            )
            .mount(&server)
            .await;
        // Falling through to onboarding is the regression this pins down, so
        // assert it never happens rather than inferring it from the result.
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("companion_project"), &server);
        assert_eq!(
            store.discover_project("access-token").await.unwrap(),
            "proj-companion"
        );
    }

    #[tokio::test]
    async fn a_projectless_account_is_onboarded_by_polling_the_operation_until_done() {
        let server = MockServer::start().await;
        // No project yet, and a tier the response says is the default.
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "allowedTiers": [{"id": "standard-tier", "isDefault": true}]
            })))
            .mount(&server)
            .await;
        // The initial POST must happen exactly once: the fix this pins down
        // is that a not-done operation is polled by `name`, not re-POSTed.
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": false,
                "name": "operations/op-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Re-POSTing `onboardUser` would also match this path — mounting
        // nothing else on it means a re-POST gets wiremock's unmatched-request
        // 500 instead of a plausible-looking response, so a regression back
        // to the old re-POST loop fails loudly rather than by accident.
        Mock::given(method("GET"))
            .and(path("/v1internal/operations/op-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": false,
                "name": "operations/op-1"
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1internal/operations/op-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": true,
                "response": {"cloudaicompanionProject": "proj-onboarded"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Real (unpaused) clock: `poll_onboard_operation` sleeps between
        // polls before each GET, so the interval is shrunk to keep this test
        // fast rather than pausing tokio's clock — a paused clock races ahead
        // of the still-real socket I/O to the mock server and times the
        // whole call out before a single request lands.
        let store = store_at(temp_auth_file("onboard"), &server)
            .with_onboard_poll_interval(Duration::from_millis(1));
        let project = store.discover_project("token").await.unwrap();
        assert_eq!(project, "proj-onboarded");
        server.verify().await;
    }

    #[tokio::test]
    async fn onboarding_error_on_a_done_operation_is_surfaced_not_misreported() {
        // `google.longrunning.Operation` is `oneof result { response | error }`
        // — a `done: true` operation carrying `error` is a server-side
        // failure, and must not fall into the "completed without a project
        // id" branch, which would misreport it as a merely-missing field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": true,
                "error": {"code": 7, "message": "not entitled to onboarding"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("onboard_operation_error"), &server);
        let error = store.discover_project("token").await.unwrap_err();
        use axum::body::to_bytes;
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("not entitled to onboarding"),
            "expected the operation error message in the response, got: {body}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn onboarding_without_an_operation_name_fails_immediately() {
        // A not-done response with no `name` has nothing to poll — that is a
        // protocol violation, not something to retry into. No poll mock is
        // mounted at all: if the fix regressed into polling anyway, the
        // unmatched GET would surface as a distinct "poll failed with HTTP
        // status 500" error instead of the message asserted below, so this
        // stays non-vacuous even without asserting elapsed time.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": false})))
            .expect(1)
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("onboard_no_name"), &server);
        let error = store.discover_project("token").await.unwrap_err();
        use axum::body::to_bytes;
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("no name to poll"),
            "expected the no-name protocol violation to be reported, got: {body}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn an_onboarding_rejection_is_reported_rather_than_polled_to_exhaustion() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(403).set_body_string("not entitled"))
            .mount(&server)
            .await;

        let store = store_at(temp_auth_file("onboard_403"), &server);
        let error = store.discover_project("token").await.unwrap_err();
        assert_eq!(error.message, "authentication failed");
    }

    #[tokio::test]
    async fn a_valid_credential_is_used_without_refreshing() {
        let server = MockServer::start().await;
        // No /token mock is mounted: a refresh attempt would 404 and fail the
        // call, so reaching the assertion proves none was made.
        let path_buf = temp_auth_file("no_refresh");
        write(
            &path_buf,
            &StoredAuth {
                access_token: "live-token".to_string(),
                refresh_token: "refresh".to_string(),
                expiry_date: Some(future_millis(3600)),
                email: Some("a@example.com".to_string()),
                project_id: Some("proj-cached".to_string()),
            },
        );

        let store = store_at(path_buf, &server);
        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.access_token, "live-token");
        // The persisted project id keeps discovery off the request path.
        assert_eq!(cred.project_id, "proj-cached");
    }

    #[tokio::test]
    async fn discovered_project_id_is_persisted_and_avoids_rediscovery() {
        // Exactly one discovery call is allowed: a second call from a fresh
        // store reading the same file would prove the persistence write did
        // not actually land.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-discovered"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("persist_discovery");
        write(
            &path_buf,
            &StoredAuth {
                access_token: "live-token".to_string(),
                refresh_token: "refresh".to_string(),
                expiry_date: Some(future_millis(3600)),
                email: Some("a@example.com".to_string()),
                project_id: None,
            },
        );

        let store = store_at(path_buf.clone(), &server);
        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.project_id, "proj-discovered");

        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        assert_eq!(written.project_id.as_deref(), Some("proj-discovered"));
        // Every other field on disk must survive the persistence write intact.
        assert_eq!(written.access_token, "live-token");
        assert_eq!(written.refresh_token, "refresh");
        assert_eq!(written.email.as_deref(), Some("a@example.com"));

        // A brand-new store (no warm in-memory project_cache) reading the same
        // file must find the persisted project id rather than discovering
        // again — `server`'s mock above is capped at exactly one call and will
        // fail the test on drop if a second request lands.
        let second_store = store_at(path_buf, &server);
        let second_cred = second_store.get_valid().await.unwrap();
        assert_eq!(second_cred.project_id, "proj-discovered");
    }

    #[tokio::test]
    async fn project_id_writeback_does_not_clobber_a_concurrent_refresh() {
        // `stored` is a snapshot `project_id`'s caller took before
        // `discover_project` ran, and that call can run for minutes on
        // first-time onboarding. If a concurrent request refreshes the
        // credential during that window, writing `stored` back verbatim
        // (plus `project_id`) would silently discard the successful refresh
        // by restoring the stale token pair — costing the operator a full
        // re-login for a lost rotated refresh token. This pins the fix: the
        // write must be based on a fresh re-read, not the stale snapshot.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-discovered"})),
            )
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("writeback_race");
        // The stale snapshot `project_id` is called with, as if read at the
        // very start of `get_valid` before `discover_project` began.
        let stale_snapshot = StoredAuth {
            access_token: "stale-access".to_string(),
            refresh_token: "stale-refresh".to_string(),
            expiry_date: Some(future_millis(3600)),
            email: Some("a@example.com".to_string()),
            project_id: None,
        };
        write(&path_buf, &stale_snapshot);

        // Simulate a concurrent refresh landing on disk while discovery is
        // still in flight against the stale snapshot above.
        let refreshed = StoredAuth {
            access_token: "fresh-access".to_string(),
            refresh_token: "fresh-refresh".to_string(),
            expiry_date: Some(future_millis(7200)),
            email: Some("a@example.com".to_string()),
            project_id: None,
        };
        write(&path_buf, &refreshed);

        let store = store_at(path_buf.clone(), &server);
        let project = store.project_id(&stale_snapshot).await.unwrap();
        assert_eq!(project, "proj-discovered");

        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        // The refreshed token pair must survive — not the stale snapshot's.
        assert_eq!(written.access_token, "fresh-access");
        assert_eq!(written.refresh_token, "fresh-refresh");
        assert_eq!(written.project_id.as_deref(), Some("proj-discovered"));
    }

    #[tokio::test]
    async fn project_id_writeback_skips_a_credential_replaced_by_another_account() {
        // Re-reading before the write keeps a concurrent *refresh* intact, but
        // it does not make the write correct when the file was replaced by a
        // different account: `shunt login antigravity` rewrites the record
        // wholesale, so merging in a project discovered for the previous
        // account would pair the new access token with the old account's
        // project on the next request.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-of-account-a"})),
            )
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("writeback_relogin");
        let stale_snapshot = StoredAuth {
            access_token: "a-access".to_string(),
            refresh_token: "a-refresh".to_string(),
            expiry_date: Some(future_millis(3600)),
            email: Some("a@example.com".to_string()),
            project_id: None,
        };
        write(&path_buf, &stale_snapshot);

        // A login for a different account lands while discovery is in flight.
        let relogged_in = StoredAuth {
            access_token: "b-access".to_string(),
            refresh_token: "b-refresh".to_string(),
            expiry_date: Some(future_millis(7200)),
            email: Some("b@example.com".to_string()),
            project_id: None,
        };
        write(&path_buf, &relogged_in);

        let store = store_at(path_buf.clone(), &server);
        // The caller still gets the project it discovered for account A.
        let project = store.project_id(&stale_snapshot).await.unwrap();
        assert_eq!(project, "proj-of-account-a");

        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        assert_eq!(written.email.as_deref(), Some("b@example.com"));
        assert_eq!(
            written.project_id, None,
            "account B's credential must not inherit account A's project id"
        );
    }

    #[tokio::test]
    async fn project_id_writeback_skips_an_account_swap_without_emails() {
        // Same replacement, but neither record carries an email — a login that
        // could not resolve one. The refresh token is the fallback anchor: a
        // login always mints a new one, so a changed value is enough to hold
        // the write back.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-of-account-a"})),
            )
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("writeback_relogin_no_email");
        let stale_snapshot = StoredAuth {
            access_token: "a-access".to_string(),
            refresh_token: "a-refresh".to_string(),
            expiry_date: Some(future_millis(3600)),
            email: None,
            project_id: None,
        };
        write(&path_buf, &stale_snapshot);

        let relogged_in = StoredAuth {
            access_token: "b-access".to_string(),
            refresh_token: "b-refresh".to_string(),
            expiry_date: Some(future_millis(7200)),
            email: None,
            project_id: None,
        };
        write(&path_buf, &relogged_in);

        let store = store_at(path_buf.clone(), &server);
        assert_eq!(
            store.project_id(&stale_snapshot).await.unwrap(),
            "proj-of-account-a"
        );

        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        assert_eq!(written.refresh_token, "b-refresh");
        assert_eq!(written.project_id, None);
    }

    #[tokio::test]
    async fn project_id_writeback_does_not_overwrite_a_freshly_resolved_project() {
        // A login (or another in-flight discovery) resolved a project id while
        // this call was discovering one. That value is at least as fresh as
        // ours, so it must survive rather than be replaced.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-discovered"})),
            )
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("writeback_existing_project");
        let stale_snapshot = StoredAuth {
            access_token: "stale-access".to_string(),
            refresh_token: "stale-refresh".to_string(),
            expiry_date: Some(future_millis(3600)),
            email: Some("a@example.com".to_string()),
            project_id: None,
        };
        write(&path_buf, &stale_snapshot);

        let with_project = StoredAuth {
            project_id: Some("proj-from-login".to_string()),
            ..stale_snapshot.clone()
        };
        write(&path_buf, &with_project);

        let store = store_at(path_buf.clone(), &server);
        assert_eq!(
            store.project_id(&stale_snapshot).await.unwrap(),
            "proj-discovered"
        );

        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        assert_eq!(written.project_id.as_deref(), Some("proj-from-login"));
    }

    #[tokio::test]
    async fn project_id_writeback_skips_persistence_when_the_file_is_gone() {
        // If the on-disk record disappeared between the stale snapshot and
        // the writeback, resurrecting it from the stale snapshot could
        // restore a credential a concurrent login or refresh has already
        // superseded. The safe choice is to skip the persist — the caller
        // still has a working access token and project id in hand, and the
        // next request simply rediscovers.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject": "proj-discovered"})),
            )
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("writeback_gone");
        let stale_snapshot = StoredAuth {
            access_token: "stale-access".to_string(),
            refresh_token: "stale-refresh".to_string(),
            expiry_date: Some(future_millis(3600)),
            email: None,
            project_id: None,
        };
        // Deliberately never written: the credential file does not exist.

        let store = store_at(path_buf.clone(), &server);
        let project = store.project_id(&stale_snapshot).await.unwrap();
        assert_eq!(project, "proj-discovered");
        assert!(
            !path_buf.exists(),
            "must not resurrect a credential file from the stale snapshot"
        );
    }

    #[tokio::test]
    async fn a_stale_credential_is_refreshed_and_persisted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "fresh-token",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("refresh");
        write(
            &path_buf,
            &StoredAuth {
                access_token: "stale-token".to_string(),
                refresh_token: "refresh-1".to_string(),
                expiry_date: Some(1),
                email: None,
                project_id: Some("proj".to_string()),
            },
        );

        let store = store_at(path_buf.clone(), &server);
        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.access_token, "fresh-token");

        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        assert_eq!(written.access_token, "fresh-token");
        // Google omits the refresh token on a plain refresh; dropping the
        // existing one would leave the next refresh with nothing to present.
        assert_eq!(written.refresh_token, "refresh-1");
        assert!(written
            .expiry_date
            .is_some_and(|at| at > future_millis(3000)));
        assert_eq!(written.project_id.as_deref(), Some("proj"));
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_replaces_the_stored_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "fresh-token",
                "refresh_token": "refresh-2",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("rotate");
        write(
            &path_buf,
            &StoredAuth {
                access_token: "stale".to_string(),
                refresh_token: "refresh-1".to_string(),
                expiry_date: Some(1),
                email: None,
                project_id: Some("proj".to_string()),
            },
        );

        store_at(path_buf.clone(), &server)
            .get_valid()
            .await
            .unwrap();
        let written: StoredAuth =
            serde_json::from_str(&fs::read_to_string(&path_buf).unwrap()).unwrap();
        assert_eq!(written.refresh_token, "refresh-2");
    }

    #[tokio::test]
    async fn a_missing_credential_file_names_the_login_command() {
        use axum::body::to_bytes;
        let server = MockServer::start().await;
        let store = store_at(
            temp_auth_file("missing").with_file_name("absent.json"),
            &server,
        );

        let error = store.get_valid().await.unwrap_err();
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("shunt login antigravity"),
            "the error must point at the login command, got: {body}"
        );
    }

    #[tokio::test]
    async fn a_rejected_refresh_points_at_re_authentication() {
        use axum::body::to_bytes;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid_grant"))
            .mount(&server)
            .await;

        let path_buf = temp_auth_file("revoked");
        write(
            &path_buf,
            &StoredAuth {
                access_token: "stale".to_string(),
                refresh_token: "revoked".to_string(),
                expiry_date: Some(1),
                email: None,
                project_id: Some("proj".to_string()),
            },
        );

        let error = store_at(path_buf, &server).get_valid().await.unwrap_err();
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("shunt login antigravity"),
            "a revoked refresh token must point at re-authentication, got: {body}"
        );
        // The upstream body must not be echoed to the client.
        assert!(!body.contains("invalid_grant"), "body: {body}");
    }

    #[tokio::test]
    async fn diagnostic_body_caps_an_oversized_response_body() {
        let server = MockServer::start().await;
        let huge = "x".repeat(DIAGNOSTIC_BODY_MAX_BYTES * 4);
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string(huge.clone()))
            .mount(&server)
            .await;

        let response = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let body = diagnostic_body(Duration::from_secs(5), response).await;

        // A tight bound, not just "less than the full body": the previous
        // cap only checked *between* chunks, so it still let through
        // whatever the first chunk happened to be -- which passed this test
        // before because wiremock/hyper happened to split `huge` into chunks
        // smaller than `huge.len()` but still well over the cap. Asserting
        // the cap plus a generous allowance for the truncation marker text
        // catches that regardless of how the body happens to be chunked.
        let marker_upper_bound = 64;
        assert!(
            body.len() <= DIAGNOSTIC_BODY_MAX_BYTES + marker_upper_bound,
            "an oversized body must be bounded by the cap (plus marker), not just \
             smaller than the full body: got {} bytes",
            body.len()
        );
        assert!(
            body.contains("truncated"),
            "a capped body must say so, rather than silently reading like the whole \
             response ended there: {body}"
        );
    }

    #[tokio::test]
    async fn diagnostic_body_bounds_a_single_oversized_chunk_and_drains_the_connection() {
        // Hand-writes a raw HTTP/1.1 response so the whole oversized body goes
        // out in one `write_all` call rather than relying on however wiremock's
        // own buffering happens to chunk a response -- the shape that defeats
        // the previous "check the cap only between chunks" loop, since a single
        // chunk larger than the whole cap sailed straight through it untouched.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let oversized_len = DIAGNOSTIC_BODY_MAX_BYTES * 4;

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await; // drain the request line/headers

            let body = vec![b'x'; oversized_len];
            let mut response = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            socket.write_all(&response).await.unwrap();

            // A second request on the SAME connection (keep-alive). If
            // `diagnostic_body` left any of the first response's bytes
            // unread, they would sit ahead of this exchange and desync it.
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .await
                .unwrap();
            // Dropping the listener here means any attempt to open a *second*
            // connection (rather than reusing the pooled one) fails fast
            // instead of silently working around an undrained first one.
        });

        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(1)
            .build()
            .unwrap();
        let uri = format!("http://{addr}");

        let response = client.get(&uri).send().await.unwrap();
        let body = diagnostic_body(Duration::from_secs(5), response).await;
        let marker_upper_bound = 64;
        assert!(
            body.len() <= DIAGNOSTIC_BODY_MAX_BYTES + marker_upper_bound,
            "a single oversized chunk must still be bounded by the cap, not \
             appended whole: got {} bytes",
            body.len()
        );
        assert!(body.contains("truncated"), "{body}");

        // Proof the connection was fully drained rather than stranded: reusing
        // it for a second request succeeds.
        let second = client.get(&uri).send().await.unwrap();
        assert_eq!(
            second.text().await.unwrap(),
            "ok",
            "the pooled connection must have been reused, not left desynced by \
             an undrained first response"
        );
    }

    #[tokio::test]
    async fn diagnostic_body_returns_a_small_body_untouched() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("small error"))
            .mount(&server)
            .await;

        let response = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let body = diagnostic_body(Duration::from_secs(5), response).await;

        // A body well under the cap must come through exactly, with no
        // spurious truncation marker.
        assert_eq!(body, "small error");
    }

    #[test]
    fn accumulate_diagnostic_chunk_bounds_a_single_oversized_chunk() {
        // The bug this guards: the previous loop checked the cap only
        // *between* chunks, so one chunk larger than the whole cap sailed
        // straight through in a single step -- exactly the shape of a small
        // upstream response delivered in one read.
        let mut buf = Vec::new();
        let mut truncated = false;
        let oversized = vec![b'x'; DIAGNOSTIC_BODY_MAX_BYTES * 4];
        accumulate_diagnostic_chunk(&mut buf, &oversized, &mut truncated);
        assert_eq!(buf.len(), DIAGNOSTIC_BODY_MAX_BYTES);
        assert!(truncated);
    }

    #[test]
    fn accumulate_diagnostic_chunk_fits_several_chunks_under_the_cap() {
        let mut buf = Vec::new();
        let mut truncated = false;
        accumulate_diagnostic_chunk(&mut buf, &[b'a'; 10], &mut truncated);
        accumulate_diagnostic_chunk(&mut buf, &[b'b'; 20], &mut truncated);
        assert_eq!(buf.len(), 30);
        assert!(!truncated);
    }

    #[test]
    fn accumulate_diagnostic_chunk_keeps_truncated_set_once_the_cap_is_hit() {
        // Once truncation happens, a later chunk landing on an already-full
        // buffer must not un-mark it -- the caller keeps polling
        // `response.chunk()` after the cap is hit purely to drain the
        // connection, not to collect more of the body.
        let mut buf = vec![b'x'; DIAGNOSTIC_BODY_MAX_BYTES];
        let mut truncated = true;
        accumulate_diagnostic_chunk(&mut buf, &[b'y'; 10], &mut truncated);
        assert_eq!(buf.len(), DIAGNOSTIC_BODY_MAX_BYTES);
        assert!(truncated);
    }
}
