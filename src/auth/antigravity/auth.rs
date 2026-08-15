//! Antigravity subscription OAuth credential store.
//!
//! Antigravity reaches the same Code Assist backend the `gemini` provider uses
//! (`cloudcode-pa.googleapis.com/v1internal`), but with its own OAuth client and
//! scopes, and it identifies itself as `ideType: ANTIGRAVITY` during project
//! discovery. The two credentials are therefore not interchangeable, which is
//! why this store exists alongside [`crate::auth::google::auth`] rather than
//! extending it.
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

/// Production Code Assist backend. Project discovery and inference both go here.
pub(crate) const API_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
/// Onboarding is served from the `daily-` host, not the production one.
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
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self {
            path,
            client,
            token_url: TOKEN_URL.to_string(),
            api_endpoint: API_ENDPOINT.to_string(),
            daily_api_endpoint: DAILY_API_ENDPOINT.to_string(),
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
        let mut persisted = stored.clone();
        persisted.project_id = Some(project.clone());
        if let Err(error) = self.write(&persisted).await {
            tracing::warn!(
                "failed to persist discovered Antigravity project id: {}",
                error.message
            );
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
/// by chunk and stops once the cap is hit, so an oversized body is never
/// fully buffered. A body cut short this way is marked in the returned
/// string so the truncation itself is visible in the log line rather than
/// looking like the response actually ended there.
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
            if buf.len() >= DIAGNOSTIC_BODY_MAX_BYTES {
                truncated = true;
                break;
            }
            match response.chunk().await {
                Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
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
/// field has three spellings across the two endpoints, and `project` may be an
/// object rather than a string.
pub(crate) fn extract_project(value: &Value) -> Option<String> {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
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

        // Without the cap this would be the full `huge.len()` bytes, buffered
        // and written into the log line whole.
        assert!(
            body.len() < huge.len(),
            "an oversized body must not be buffered and logged in full: got {} bytes",
            body.len()
        );
        assert!(
            body.contains("truncated"),
            "a capped body must say so, rather than silently reading like the whole \
             response ended there: {body}"
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
}
