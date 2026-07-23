//! Read-only observation of machine-local CLI credentials for the admin usage view.
//!
//! This module deliberately never refreshes, copies, or writes a credential. It reads
//! only enough metadata to identify the login and, for Claude, returns a current access
//! token to the caller so the usage API can be queried. Token-bearing values are never
//! serialized.

use std::{fs, path::PathBuf, time::SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::auth::shared::{is_token_valid_at, jwt_claims};

const GOOGLE_LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const GOOGLE_RETRIEVE_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
const KIMI_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";
const CURSOR_USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const CURSOR_USER_URL: &str = "https://cursor.com/api/auth/me";
const CURSOR_APP_ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
const GROK_API_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const GROK_CLIENT_MODE: &str = "cli";
const ANTIGRAVITY_STATUS_PATH: &str = "/exa.language_server_pb.LanguageServerService/GetUserStatus";
const OBSERVATION_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
}

fn cursor_app_state_db_path(home: &std::path::Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        home.join("AppData")
            .join("Roaming")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }
    #[cfg(target_os = "macos")]
    {
        home.join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        home.join(".config")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct QuotaBucket {
    pub(crate) label: String,
    pub(crate) remaining: Option<f64>,
    pub(crate) remaining_amount: Option<String>,
    pub(crate) reset_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GeminiQuotaBucket {
    #[serde(rename = "modelId")]
    pub(crate) model_id: Option<String>,
    #[serde(rename = "remainingFraction")]
    pub(crate) remaining_fraction: Option<f64>,
    #[serde(rename = "remainingAmount")]
    pub(crate) remaining_amount: Option<String>,
    #[serde(rename = "resetTime")]
    pub(crate) reset_time: Option<String>,
}

#[derive(Debug)]
pub(crate) struct GeminiQuotaSnapshot {
    pub(crate) account_label: String,
    pub(crate) detail: Option<String>,
    pub(crate) buckets: Vec<GeminiQuotaBucket>,
}

#[derive(Debug)]
pub(crate) struct CursorQuotaSnapshot {
    pub(crate) account_label: String,
    pub(crate) detail: Option<String>,
    pub(crate) buckets: Vec<QuotaBucket>,
}

#[derive(Debug)]
pub(crate) struct GrokQuotaSnapshot {
    pub(crate) account_label: String,
    pub(crate) detail: Option<String>,
    pub(crate) buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Deserialize)]
struct GrokBillingResponse {
    config: GrokBillingConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrokBillingConfig {
    credit_usage_percent: Option<f64>,
    billing_period_end: Option<String>,
    #[serde(default)]
    product_usage: Vec<GrokProductUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrokProductUsage {
    product: String,
    usage_percent: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrokUserInfo {
    email: Option<String>,
    first_name: Option<String>,
    subscription_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUsageSummary {
    billing_cycle_end: Option<String>,
    membership_type: Option<String>,
    individual_usage: Option<CursorIndividualUsage>,
}

#[derive(Debug, Deserialize)]
struct CursorIndividualUsage {
    plan: Option<CursorPlanUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorPlanUsage {
    used: Option<f64>,
    limit: Option<f64>,
    auto_percent_used: Option<f64>,
    api_percent_used: Option<f64>,
    total_percent_used: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct CursorUserInfo {
    email: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiQuotaResponse {
    #[serde(default)]
    buckets: Vec<GeminiQuotaBucket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityStatusResponse {
    user_status: AntigravityUserStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityUserStatus {
    name: Option<String>,
    email: Option<String>,
    plan_status: AntigravityPlanStatus,
    cascade_model_config_data: AntigravityModelConfigData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityPlanStatus {
    plan_info: AntigravityPlanInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityPlanInfo {
    plan_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityModelConfigData {
    #[serde(default)]
    client_model_configs: Vec<AntigravityModelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityModelConfig {
    label: String,
    quota_info: Option<AntigravityQuotaInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityQuotaInfo {
    remaining_fraction: f64,
    reset_time: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedProvider {
    Claude,
    Codex,
    Grok,
    Kimi,
    Gemini,
    Cursor,
}

impl ObservedProvider {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Kimi => "kimi",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
        }
    }

    pub(crate) fn signal(self) -> &'static str {
        match self {
            Self::Claude => "quota",
            Self::Codex => "response-derived",
            Self::Gemini | Self::Kimi | Self::Cursor | Self::Grok => "quota",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedSource {
    File,
    Keychain,
}

impl ObservedSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "local credential file",
            Self::Keychain => "macOS Keychain",
        }
    }
}

#[derive(Clone)]
pub(crate) struct ObservedCredential {
    pub(crate) provider: ObservedProvider,
    pub(crate) identity: String,
    pub(crate) detail: Option<String>,
    pub(crate) source: ObservedSource,
    pub(crate) valid: bool,
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
}

pub(crate) fn discover() -> Vec<ObservedCredential> {
    [
        discover_claude(),
        discover_codex(),
        discover_grok(),
        discover_kimi(),
        discover_gemini(),
        discover_cursor(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub(crate) async fn fetch_gemini_quota(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<GeminiQuotaSnapshot, String> {
    if let Ok(connection) = tokio::task::spawn_blocking(discover_antigravity_connection)
        .await
        .map_err(|error| format!("Antigravity discovery task failed: {error}"))?
    {
        if let Ok(snapshot) = fetch_antigravity_quota_from(client, &connection).await {
            return Ok(snapshot);
        }
    }
    fetch_gemini_quota_from(
        client,
        access_token,
        GOOGLE_LOAD_CODE_ASSIST_URL,
        GOOGLE_RETRIEVE_QUOTA_URL,
    )
    .await
}

struct AntigravityConnection {
    ports: Vec<u16>,
    csrf_token: String,
}

fn discover_antigravity_connection() -> Result<AntigravityConnection, String> {
    #[cfg(not(unix))]
    {
        return Err("Antigravity local discovery is only supported on Unix".to_string());
    }
    #[cfg(unix)]
    {
        use std::process::Command;

        let processes = Command::new("ps")
            .args(["-ax", "-o", "pid=,command="])
            .output()
            .map_err(|error| format!("could not inspect Antigravity processes: {error}"))?;
        let processes = String::from_utf8_lossy(&processes.stdout);
        let line = processes
            .lines()
            .find(|line| {
                line.contains(
                    "/Applications/Antigravity.app/Contents/Resources/bin/language_server",
                ) && line.contains("--csrf_token")
            })
            .ok_or_else(|| "Antigravity is not running".to_string())?;
        let mut words = line.split_whitespace();
        let pid = words
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| "could not identify Antigravity language server".to_string())?;
        let words = words.collect::<Vec<_>>();
        let csrf_token = words
            .windows(2)
            .find(|window| window[0] == "--csrf_token")
            .map(|window| window[1].to_string())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "Antigravity language server has no CSRF token".to_string())?;

        let listeners = Command::new("lsof")
            .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
            .output()
            .map_err(|error| format!("could not inspect Antigravity listeners: {error}"))?;
        let listeners = String::from_utf8_lossy(&listeners.stdout);
        let mut ports = listeners
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .find(|part| part.contains("127.0.0.1:"))
            })
            .filter_map(|address| address.rsplit(':').next())
            .filter_map(|port| port.parse::<u16>().ok())
            .collect::<Vec<_>>();
        ports.sort_unstable();
        ports.dedup();
        if ports.is_empty() {
            return Err("Antigravity language server has no loopback listener".to_string());
        }
        Ok(AntigravityConnection { ports, csrf_token })
    }
}

async fn fetch_antigravity_quota_from(
    _client: &reqwest::Client,
    connection: &AntigravityConnection,
) -> Result<GeminiQuotaSnapshot, String> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(OBSERVATION_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("could not build Antigravity loopback client: {error}"))?;
    let mut last_error = "Antigravity quota service unavailable".to_string();
    for port in &connection.ports {
        let response = client
            .post(format!("https://127.0.0.1:{port}{ANTIGRAVITY_STATUS_PATH}"))
            .header("x-codeium-csrf-token", &connection.csrf_token)
            .header("connect-protocol-version", "1")
            .json(&serde_json::json!({}))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("Antigravity quota request failed: {error}");
                continue;
            }
        };
        if !response.status().is_success() {
            last_error = format!(
                "Antigravity quota request returned HTTP {}",
                response.status()
            );
            continue;
        }
        let status = match response.json::<AntigravityStatusResponse>().await {
            Ok(status) => status,
            Err(error) => {
                last_error = format!("invalid Antigravity quota response: {error}");
                continue;
            }
        };
        let plan = status.user_status.plan_status.plan_info.plan_name;
        let account_label = if plan.is_empty() {
            "Antigravity".to_string()
        } else {
            format!("Antigravity · Google AI {plan}")
        };
        let detail = status
            .user_status
            .name
            .filter(|value| !value.is_empty())
            .or_else(|| status.user_status.email.map(|email| mask_email(&email)));
        let buckets = status
            .user_status
            .cascade_model_config_data
            .client_model_configs
            .into_iter()
            .filter_map(|model| {
                let quota = model.quota_info?;
                Some(GeminiQuotaBucket {
                    model_id: Some(model.label),
                    remaining_fraction: Some(quota.remaining_fraction),
                    remaining_amount: None,
                    reset_time: quota.reset_time,
                })
            })
            .collect::<Vec<_>>();
        return Ok(GeminiQuotaSnapshot {
            account_label,
            detail,
            buckets,
        });
    }
    Err(last_error)
}

async fn fetch_gemini_quota_from(
    client: &reqwest::Client,
    access_token: &str,
    load_url: &str,
    quota_url: &str,
) -> Result<GeminiQuotaSnapshot, String> {
    let load = client
        .post(load_url)
        .bearer_auth(access_token)
        .json(&serde_json::json!({}))
        .timeout(OBSERVATION_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("Code Assist discovery failed: {error}"))?;
    if !load.status().is_success() {
        return Err(format!(
            "Code Assist discovery returned HTTP {}",
            load.status()
        ));
    }
    let load: Value = load
        .json()
        .await
        .map_err(|error| format!("invalid Code Assist discovery response: {error}"))?;
    let account_label = load
        .pointer("/paidTier/name")
        .or_else(|| load.pointer("/currentTier/name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(|name| name.replace(" in ", " · "))
        .unwrap_or_else(|| "Gemini Code Assist".to_string());
    let project = load
        .get("cloudaicompanionProject")
        .or_else(|| load.get("companionProject"))
        .or_else(|| load.get("projectId"))
        .and_then(Value::as_str)
        .filter(|project| !project.is_empty())
        .ok_or_else(|| "Code Assist discovery returned no project ID".to_string())?;

    let response = client
        .post(quota_url)
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "project": project }))
        .timeout(OBSERVATION_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("Gemini quota request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Gemini quota request returned HTTP {}",
            response.status()
        ));
    }
    response
        .json::<GeminiQuotaResponse>()
        .await
        .map(|response| GeminiQuotaSnapshot {
            account_label,
            detail: None,
            buckets: response.buckets,
        })
        .map_err(|error| format!("invalid Gemini quota response: {error}"))
}

#[derive(Debug, Deserialize)]
struct KimiUsageResponse {
    usage: KimiUsageDetail,
    #[serde(default)]
    limits: Vec<KimiRateLimit>,
}

#[derive(Debug, Deserialize)]
struct KimiUsageDetail {
    limit: String,
    used: Option<String>,
    remaining: Option<String>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KimiRateLimit {
    window: KimiWindow,
    detail: KimiUsageDetail,
}

#[derive(Debug, Deserialize)]
struct KimiWindow {
    duration: u64,
    #[serde(rename = "timeUnit")]
    time_unit: String,
}

pub(crate) async fn fetch_kimi_quota(
    client: &reqwest::Client,
    access_token: &str,
    sidecar_base_url: Option<&str>,
) -> Result<Vec<QuotaBucket>, String> {
    if let Some(usage_url) = sidecar_base_url.and_then(kimi_sidecar_usage_url) {
        if let Ok(buckets) = fetch_kimi_quota_from(client, None, &usage_url).await {
            return Ok(buckets);
        }
    }
    fetch_kimi_quota_from(client, Some(access_token), KIMI_USAGE_URL).await
}

fn kimi_sidecar_usage_url(base_url: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(base_url).ok()?;
    if url.scheme() != "http" || !url.host_str().is_some_and(crate::config::host_is_loopback) {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/v1/usages"));
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

async fn fetch_kimi_quota_from(
    client: &reqwest::Client,
    access_token: Option<&str>,
    usage_url: &str,
) -> Result<Vec<QuotaBucket>, String> {
    let mut request = client
        .get(usage_url)
        .header("accept", "application/json")
        .timeout(OBSERVATION_REQUEST_TIMEOUT);
    if let Some(access_token) = access_token {
        request = request.bearer_auth(access_token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Kimi quota request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Kimi quota request returned HTTP {}",
            response.status()
        ));
    }
    let response = response
        .json::<KimiUsageResponse>()
        .await
        .map_err(|error| format!("invalid Kimi quota response: {error}"))?;
    let mut buckets = vec![kimi_bucket("Week", response.usage)];
    buckets.extend(response.limits.into_iter().map(|limit| {
        let label = if limit.window.duration == 300
            && limit
                .window
                .time_unit
                .eq_ignore_ascii_case("TIME_UNIT_MINUTE")
        {
            "5h".to_string()
        } else {
            format!("{} {}", limit.window.duration, limit.window.time_unit)
        };
        kimi_bucket(&label, limit.detail)
    }));
    Ok(buckets)
}

fn kimi_bucket(label: &str, detail: KimiUsageDetail) -> QuotaBucket {
    let limit = detail.limit.parse::<f64>().ok();
    let used = detail
        .used
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok());
    let remaining_value = detail
        .remaining
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok());
    QuotaBucket {
        label: label.to_string(),
        remaining: limit.filter(|limit| *limit > 0.0).and_then(|limit| {
            remaining_value
                .map(|remaining| remaining / limit)
                .or_else(|| used.map(|used| 1.0 - used / limit))
        }),
        remaining_amount: detail.remaining,
        reset_time: detail.reset_time,
    }
}

pub(crate) async fn fetch_cursor_quota(
    client: &reqwest::Client,
) -> Result<CursorQuotaSnapshot, String> {
    let session = tokio::task::spawn_blocking(read_cursor_app_session)
        .await
        .map_err(|error| format!("Cursor app session read task failed: {error}"))??;
    fetch_cursor_quota_from(client, &session.cookie, CURSOR_USAGE_URL, CURSOR_USER_URL).await
}

struct CursorAppSession {
    cookie: String,
}

fn read_cursor_app_session() -> Result<CursorAppSession, String> {
    let home = user_home().ok_or_else(|| "HOME and USERPROFILE are unset".to_string())?;
    let path = cursor_app_state_db_path(&home);
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("could not open Cursor app state read-only: {error}"))?;
    connection
        .busy_timeout(std::time::Duration::from_millis(250))
        .map_err(|error| format!("could not configure Cursor app state read: {error}"))?;
    let access_token: String = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1",
            [CURSOR_APP_ACCESS_TOKEN_KEY],
            |row| row.get(0),
        )
        .map_err(|error| format!("Cursor app access token unavailable: {error}"))?;
    cursor_app_session(&access_token, SystemTime::now())
        .ok_or_else(|| "Cursor app session is expired or malformed".to_string())
}

fn cursor_app_session(access_token: &str, now: SystemTime) -> Option<CursorAppSession> {
    if !is_token_valid_at(access_token, now) {
        return None;
    }
    let claims = jwt_claims(access_token)?;
    let subject = claims.get("sub")?.as_str()?;
    let user_id = subject.rsplit('|').next()?.trim();
    if user_id.is_empty()
        || !user_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return None;
    }
    Some(CursorAppSession {
        cookie: format!("WorkosCursorSessionToken={user_id}%3A%3A{access_token}"),
    })
}

async fn fetch_cursor_quota_from(
    client: &reqwest::Client,
    cookie: &str,
    usage_url: &str,
    user_url: &str,
) -> Result<CursorQuotaSnapshot, String> {
    let usage_request = client
        .get(usage_url)
        .header("accept", "application/json")
        .header("cookie", cookie)
        .send();
    let user_request = client
        .get(user_url)
        .header("accept", "application/json")
        .header("cookie", cookie)
        .send();
    let (usage_response, user_response) = tokio::join!(usage_request, user_request);
    let usage_response =
        usage_response.map_err(|error| format!("Cursor usage request failed: {error}"))?;
    if !usage_response.status().is_success() {
        return Err(format!(
            "Cursor usage request returned HTTP {}",
            usage_response.status()
        ));
    }
    let usage = usage_response
        .json::<CursorUsageSummary>()
        .await
        .map_err(|error| format!("invalid Cursor usage response: {error}"))?;
    let user = match user_response {
        Ok(response) if response.status().is_success() => {
            response.json::<CursorUserInfo>().await.ok()
        }
        _ => None,
    };
    Ok(cursor_quota_snapshot(usage, user))
}

fn cursor_quota_snapshot(
    usage: CursorUsageSummary,
    user: Option<CursorUserInfo>,
) -> CursorQuotaSnapshot {
    let plan = usage.individual_usage.and_then(|usage| usage.plan);
    let ratio = plan
        .as_ref()
        .and_then(|plan| match (plan.used, plan.limit) {
            (Some(used), Some(limit)) if limit > 0.0 => Some(used / limit),
            _ => None,
        });
    let mut buckets = Vec::new();
    for (label, used_fraction) in [
        (
            "Billing cycle",
            plan.as_ref()
                .and_then(|plan| plan.total_percent_used.map(|value| value / 100.0))
                .or(ratio),
        ),
        (
            "Auto + Composer",
            plan.as_ref()
                .and_then(|plan| plan.auto_percent_used.map(|value| value / 100.0)),
        ),
        (
            "Named models",
            plan.as_ref()
                .and_then(|plan| plan.api_percent_used.map(|value| value / 100.0)),
        ),
    ] {
        if let Some(used_fraction) = used_fraction {
            buckets.push(QuotaBucket {
                label: label.to_string(),
                remaining: Some((1.0 - used_fraction).clamp(0.0, 1.0)),
                remaining_amount: None,
                reset_time: usage.billing_cycle_end.clone(),
            });
        }
    }
    let membership = usage
        .membership_type
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(title_case)
        .unwrap_or_else(|| "Subscription".to_string());
    let account_label = format!("Cursor · {membership}");
    let detail = user.and_then(|user| {
        user.name
            .filter(|value| !value.is_empty())
            .or_else(|| user.email.map(|email| mask_email(&email)))
    });
    CursorQuotaSnapshot {
        account_label,
        detail,
        buckets,
    }
}

pub(crate) async fn fetch_grok_quota(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<GrokQuotaSnapshot, String> {
    fetch_grok_quota_from(client, access_token, GROK_API_BASE_URL).await
}

async fn fetch_grok_quota_from(
    client: &reqwest::Client,
    access_token: &str,
    base_url: &str,
) -> Result<GrokQuotaSnapshot, String> {
    let base_url = base_url.trim_end_matches('/');
    let billing_request = client
        .get(format!("{base_url}/billing?format=credits"))
        .bearer_auth(access_token)
        .header("x-grok-client-mode", GROK_CLIENT_MODE)
        .header("accept", "application/json")
        .timeout(OBSERVATION_REQUEST_TIMEOUT)
        .send();
    let user_request = client
        .get(format!("{base_url}/user?include=subscription"))
        .bearer_auth(access_token)
        .header("x-grok-client-mode", GROK_CLIENT_MODE)
        .header("accept", "application/json")
        .timeout(OBSERVATION_REQUEST_TIMEOUT)
        .send();
    let (billing_response, user_response) = tokio::join!(billing_request, user_request);
    let billing_response =
        billing_response.map_err(|error| format!("Grok billing request failed: {error}"))?;
    if !billing_response.status().is_success() {
        return Err(format!(
            "Grok billing request returned HTTP {}",
            billing_response.status()
        ));
    }
    let billing = billing_response
        .json::<GrokBillingResponse>()
        .await
        .map_err(|error| format!("invalid Grok billing response: {error}"))?;
    let user = match user_response {
        Ok(response) if response.status().is_success() => {
            response.json::<GrokUserInfo>().await.ok()
        }
        _ => None,
    };
    Ok(grok_quota_snapshot(billing.config, user))
}

fn grok_quota_snapshot(
    billing: GrokBillingConfig,
    user: Option<GrokUserInfo>,
) -> GrokQuotaSnapshot {
    let reset_time = billing.billing_period_end;
    let mut buckets = Vec::new();
    if let Some(used_percent) = billing.credit_usage_percent {
        buckets.push(QuotaBucket {
            label: "Credits".to_string(),
            remaining: Some((1.0 - used_percent / 100.0).clamp(0.0, 1.0)),
            remaining_amount: None,
            reset_time: reset_time.clone(),
        });
    }
    buckets.extend(
        billing
            .product_usage
            .into_iter()
            .map(|product| QuotaBucket {
                label: normalize_grok_name(&product.product),
                remaining: Some((1.0 - product.usage_percent / 100.0).clamp(0.0, 1.0)),
                remaining_amount: None,
                reset_time: reset_time.clone(),
            }),
    );
    let plan = user
        .as_ref()
        .and_then(|user| user.subscription_tier.as_deref())
        .filter(|value| !value.is_empty())
        .map(normalize_grok_name)
        .unwrap_or_else(|| "Subscription".to_string());
    let detail = user.and_then(|user| {
        user.first_name
            .filter(|value| !value.is_empty())
            .or_else(|| user.email.map(|email| mask_email(&email)))
    });
    GrokQuotaSnapshot {
        account_label: format!("Grok · {plan}"),
        detail,
        buckets,
    }
}

fn discover_claude() -> Option<ObservedCredential> {
    let path = crate::auth::claude::auth::default_credentials_path();
    if let Some(value) = read_json(&path) {
        return parse_claude(&value, ObservedSource::File, SystemTime::now());
    }
    read_claude_keychain()
        .as_ref()
        .and_then(|value| parse_claude(value, ObservedSource::Keychain, SystemTime::now()))
}

fn discover_codex() -> Option<ObservedCredential> {
    let path = crate::auth::default_codex_auth_path();
    let value = read_json(&path)?;
    parse_codex(&value, SystemTime::now())
}

fn discover_grok() -> Option<ObservedCredential> {
    let home = user_home()?;
    let value = read_json(&home.join(".grok").join("auth.json"))?;
    parse_grok_cli(&value, SystemTime::now())
}

fn parse_grok_cli(value: &Value, now: SystemTime) -> Option<ObservedCredential> {
    let entry = value
        .as_object()?
        .iter()
        .filter_map(|(scope, value)| value.as_object().map(|entry| (scope, entry)))
        .filter(|(_, entry)| entry.get("key").and_then(Value::as_str).is_some())
        .min_by_key(|(scope, _)| !scope.starts_with("https://auth.x.ai::"))?
        .1;
    let access_token = entry.get("key")?.as_str()?.to_string();
    let email = entry
        .get("email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(mask_email);
    let auth_mode = entry
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let plan = if auth_mode.eq_ignore_ascii_case("oidc") {
        "SuperGrok"
    } else {
        "Subscription"
    };
    Some(ObservedCredential {
        provider: ObservedProvider::Grok,
        identity: format!("Grok CLI · {plan}"),
        detail: email,
        source: ObservedSource::File,
        valid: is_token_valid_at(&access_token, now),
        access_token,
        account_id: None,
    })
}

fn discover_kimi() -> Option<ObservedCredential> {
    let home = user_home()?;
    let value = read_json(
        &home
            .join(".kimi-code")
            .join("credentials")
            .join("kimi-code.json"),
    )?;
    parse_oauth_provider(
        &value,
        ObservedProvider::Kimi,
        "Kimi Code",
        &["/access_token"],
        SystemTime::now(),
    )
}

fn discover_gemini() -> Option<ObservedCredential> {
    if discover_antigravity_connection().is_ok() {
        return Some(ObservedCredential {
            provider: ObservedProvider::Gemini,
            identity: "Antigravity".to_string(),
            detail: Some("Google AI subscription".to_string()),
            source: ObservedSource::File,
            valid: true,
            access_token: String::new(),
            account_id: None,
        });
    }
    let home = user_home()?;
    let value = read_json(&home.join(".gemini").join("oauth_creds.json"))?;
    let mut observed = parse_oauth_provider(
        &value,
        ObservedProvider::Gemini,
        "Gemini Code Assist · Google One AI Pro",
        &["/id_token", "/access_token"],
        SystemTime::now(),
    )?;
    if observed.identity != "Gemini Code Assist · Google One AI Pro" {
        observed.detail = Some(observed.identity);
        observed.identity = "Gemini Code Assist · Google One AI Pro".to_string();
    }
    Some(observed)
}

fn discover_cursor() -> Option<ObservedCredential> {
    let mut observed = read_json(&crate::auth::default_cursor_auth_path())
        .as_ref()
        .and_then(parse_cursor)
        .or_else(discover_cursor_app)?;
    if let Some((name, email)) = read_cursor_identity() {
        observed.identity = format!("Cursor · {name}");
        observed.detail = Some(mask_email(&email));
    }
    Some(observed)
}

fn discover_cursor_app() -> Option<ObservedCredential> {
    let session = read_cursor_app_session().ok()?;
    (!session.cookie.is_empty()).then_some(ObservedCredential {
        provider: ObservedProvider::Cursor,
        identity: "Cursor account".to_string(),
        detail: Some("Cursor.app login".to_string()),
        source: ObservedSource::File,
        valid: true,
        access_token: String::new(),
        account_id: None,
    })
}

fn read_cursor_identity() -> Option<(String, String)> {
    use std::process::Command;

    let output = Command::new("cursor-agent")
        .args(["status", "--format", "json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    let user = value.get("userInfo")?;
    let email = user.get("email")?.as_str()?.to_string();
    let first = user
        .get("firstName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let last = user
        .get("lastName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = format!("{first} {last}").trim().to_string();
    Some((if name.is_empty() { email.clone() } else { name }, email))
}

fn parse_cursor(value: &Value) -> Option<ObservedCredential> {
    let access_token = value.get("accessToken")?.as_str()?.to_string();
    let claims = jwt_claims(&access_token);
    let identity = claims
        .as_ref()
        .and_then(|claims| {
            claims
                .get("email")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(mask_email)
                .or_else(|| {
                    claims
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(ToOwned::to_owned)
                })
        })
        .unwrap_or_else(|| "Cursor account".to_string());
    Some(ObservedCredential {
        provider: ObservedProvider::Cursor,
        identity,
        detail: Some("Cursor subscription".to_string()),
        source: ObservedSource::File,
        valid: claims
            .as_ref()
            .and_then(|claims| claims.get("exp"))
            .is_none_or(|_| is_token_valid_at(&access_token, SystemTime::now())),
        access_token,
        account_id: None,
    })
}

fn parse_oauth_provider(
    value: &Value,
    provider: ObservedProvider,
    fallback_identity: &str,
    token_pointers: &[&str],
    now: SystemTime,
) -> Option<ObservedCredential> {
    let tokens = token_pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let claims = tokens.iter().find_map(|token| jwt_claims(token));
    let access_token = tokens.last().copied().unwrap_or_default().to_string();
    let identity = claims
        .as_ref()
        .and_then(masked_claim_identity)
        .unwrap_or_else(|| fallback_identity.to_string());
    let valid = claims
        .as_ref()
        .and_then(|claims| claims.get("exp"))
        .and_then(Value::as_i64)
        .map(|expires| {
            (now.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64)
                < expires
        })
        .unwrap_or(!access_token.is_empty());
    Some(ObservedCredential {
        provider,
        identity,
        detail: Some(fallback_identity.to_string()),
        source: ObservedSource::File,
        valid,
        access_token,
        account_id: None,
    })
}

fn masked_claim_identity(claims: &Value) -> Option<String> {
    claims
        .get("email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(mask_email)
        .or_else(|| {
            claims
                .get("sub")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(mask_id)
        })
}

fn read_json(path: &PathBuf) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(target_os = "macos")]
fn read_claude_keychain() -> Option<Value> {
    use std::process::Command;

    const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

#[cfg(not(target_os = "macos"))]
fn read_claude_keychain() -> Option<Value> {
    None
}

fn parse_claude(
    value: &Value,
    source: ObservedSource,
    now: SystemTime,
) -> Option<ObservedCredential> {
    let oauth = value.get("claudeAiOauth")?;
    let access_token = oauth.get("accessToken")?.as_str()?.to_string();
    let expires_at_ms = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let now_ms = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);
    let plan = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(title_case);
    let identity = plan
        .as_deref()
        .map(|plan| format!("Claude Code · {plan}"))
        .unwrap_or_else(|| "Claude Code".to_string());

    Some(ObservedCredential {
        provider: ObservedProvider::Claude,
        identity,
        detail: plan.map(|plan| format!("{plan} plan")),
        source,
        valid: expires_at_ms > now_ms,
        access_token,
        account_id: None,
    })
}

fn parse_codex(value: &Value, now: SystemTime) -> Option<ObservedCredential> {
    if !value
        .get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("chatgpt"))
    {
        return None;
    }
    let tokens = value.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?.to_string();
    let claims = jwt_claims(&access_token);
    let identity_claims = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims)
        .or_else(|| claims.clone());
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            identity_claims
                .as_ref()?
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")?
                .as_str()
        })
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let email = identity_claims
        .as_ref()
        .and_then(|claims| claims.get("email"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(mask_email);
    let plan = identity_claims
        .as_ref()
        .and_then(|claims| claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(normalize_chatgpt_plan);
    let identity = plan
        .as_deref()
        .map(|plan| format!("ChatGPT · {plan}"))
        .unwrap_or_else(|| "ChatGPT".to_string());
    let detail = email.or_else(|| account_id.as_deref().map(mask_id));

    Some(ObservedCredential {
        provider: ObservedProvider::Codex,
        identity,
        detail,
        source: ObservedSource::File,
        valid: is_token_valid_at(&access_token, now),
        access_token,
        account_id,
    })
}

fn normalize_chatgpt_plan(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "prolite" | "pro" => "Pro".to_string(),
        "plus" => "Plus".to_string(),
        "team" => "Team".to_string(),
        "enterprise" => "Enterprise".to_string(),
        other => title_case(other),
    }
}

fn normalize_grok_name(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "grokpro" | "grok_pro" | "grok-pro" => "Grok Pro".to_string(),
        "supergrok" | "super_grok" | "super-grok" => "SuperGrok".to_string(),
        "grokbuild" | "grok_build" | "grok-build" => "Grok Build".to_string(),
        other => title_case(&other.replace(['_', '-'], " ")),
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn mask_email(value: &str) -> String {
    let Some((local, domain)) = value.split_once('@') else {
        return mask_id(value);
    };
    let first = local.chars().next().unwrap_or('•');
    format!("{first}•••@{domain}")
}

fn mask_id(value: &str) -> String {
    if value.chars().count() <= 8 {
        return value.to_string();
    }
    let start: String = value.chars().take(4).collect();
    let end: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;

    use super::*;

    fn jwt(claims: Value) -> String {
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn parses_claude_identity_without_exposing_refresh_token() {
        let value = json!({"claudeAiOauth": {
            "accessToken": "access", "refreshToken": "must-not-escape",
            "expiresAt": 2_000_000, "subscriptionType": "max"
        }});
        let observed = parse_claude(
            &value,
            ObservedSource::File,
            UNIX_EPOCH + Duration::from_millis(1_000_000),
        )
        .unwrap();

        assert_eq!(observed.identity, "Claude Code · Max");
        assert_eq!(observed.detail.as_deref(), Some("Max plan"));
        assert_eq!(observed.access_token, "access");
        assert!(observed.valid);
    }

    #[test]
    fn parses_and_masks_codex_identity() {
        let access = jwt(json!({
            "exp": 2_000,
            "email": "rubén@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123456789",
                "chatgpt_plan_type": "prolite"
            }
        }));
        let value = json!({"auth_mode": "ChatGPT", "tokens": {
            "access_token": access, "refresh_token": "must-not-escape"
        }});
        let observed = parse_codex(&value, UNIX_EPOCH + Duration::from_secs(1_000)).unwrap();

        assert_eq!(observed.identity, "ChatGPT · Pro");
        assert_eq!(observed.detail.as_deref(), Some("r•••@example.com"));
        assert_eq!(observed.account_id.as_deref(), Some("acct_123456789"));
        assert!(observed.valid);
    }

    #[test]
    fn marks_expired_credentials_unavailable_without_refreshing() {
        let access = jwt(json!({"exp": 1_000}));
        let value = json!({"auth_mode": "ChatGPT", "tokens": {"access_token": access}});
        let observed = parse_codex(&value, UNIX_EPOCH + Duration::from_secs(1_500)).unwrap();

        assert!(!observed.valid);
    }

    #[test]
    fn accepts_real_codex_lowercase_auth_mode() {
        let access = jwt(json!({"exp": 2_000, "email": "user@example.com"}));
        let value = json!({"auth_mode": "chatgpt", "tokens": {"access_token": access}});

        assert!(parse_codex(&value, UNIX_EPOCH + Duration::from_secs(1_000)).is_some());
    }

    #[test]
    fn refreshable_provider_is_expired_when_current_access_token_is_expired() {
        let expired = jwt(json!({"exp": 1_000, "email": "user@example.com"}));
        let value = json!({"access_token": expired, "refresh_token": "refresh"});
        let observed = parse_oauth_provider(
            &value,
            ObservedProvider::Gemini,
            "Gemini CLI",
            &["/id_token", "/access_token"],
            UNIX_EPOCH + Duration::from_secs(1_500),
        )
        .unwrap();

        assert!(!observed.valid);
    }

    #[test]
    fn cursor_does_not_present_opaque_subject_as_identity() {
        let access = jwt(json!({"exp": 2_000, "sub": "google-oauth2|opaque"}));
        let observed = parse_cursor(&json!({"accessToken": access})).unwrap();

        assert_eq!(observed.identity, "Cursor account");
    }

    #[tokio::test]
    async fn fetches_gemini_model_quota_read_only() {
        use wiremock::matchers::{bearer_token, body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/load"))
            .and(bearer_token("access"))
            .and(body_json(json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "cloudaicompanionProject": "project-123",
                "paidTier": {"name": "Gemini Code Assist in Google One AI Pro"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/quota"))
            .and(bearer_token("access"))
            .and(body_json(json!({"project": "project-123"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "buckets": [{
                    "modelId": "gemini-2.5-pro",
                    "remainingFraction": 0.75,
                    "remainingAmount": "750",
                    "resetTime": "2026-07-23T18:23:50Z"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let snapshot = fetch_gemini_quota_from(
            &reqwest::Client::new(),
            "access",
            &format!("{}/load", server.uri()),
            &format!("{}/quota", server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(
            snapshot.account_label,
            "Gemini Code Assist · Google One AI Pro"
        );
        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(
            snapshot.buckets[0].model_id.as_deref(),
            Some("gemini-2.5-pro")
        );
        assert_eq!(snapshot.buckets[0].remaining_fraction, Some(0.75));
        assert_eq!(snapshot.buckets[0].remaining_amount.as_deref(), Some("750"));
    }

    #[tokio::test]
    async fn fetches_kimi_weekly_and_five_hour_quota_read_only() {
        use wiremock::matchers::{bearer_token, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usages"))
            .and(bearer_token("access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "usage": {
                    "limit": "100", "used": "60", "remaining": "40",
                    "resetTime": "2026-07-28T09:34:16Z"
                },
                "limits": [{
                    "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                    "detail": {
                        "limit": "100", "used": "17", "remaining": "83",
                        "resetTime": "2026-07-23T01:34:16Z"
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let buckets = fetch_kimi_quota_from(
            &reqwest::Client::new(),
            Some("access"),
            &format!("{}/usages", server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].label, "Week");
        assert_eq!(buckets[0].remaining, Some(0.4));
        assert_eq!(buckets[1].label, "5h");
        assert_eq!(buckets[1].remaining, Some(0.83));
    }

    #[tokio::test]
    async fn fetches_kimi_quota_without_bearer_for_provider_sidecar() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/coding/v1/usages"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "usage": {
                    "limit": "100", "used": "20", "remaining": "80",
                    "resetTime": "2026-07-28T09:34:16Z"
                },
                "limits": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let buckets = fetch_kimi_quota_from(
            &reqwest::Client::new(),
            None,
            &format!("{}/coding/v1/usages", server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(buckets[0].remaining, Some(0.8));
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get("authorization").is_none());
    }

    #[test]
    fn derives_kimi_sidecar_usage_only_from_configured_loopback_url() {
        assert_eq!(
            kimi_sidecar_usage_url("http://127.0.0.1:3011/coding").as_deref(),
            Some("http://127.0.0.1:3011/coding/v1/usages")
        );
        assert!(kimi_sidecar_usage_url("https://api.kimi.com/coding").is_none());
        assert!(kimi_sidecar_usage_url("http://example.com/coding").is_none());
    }

    #[tokio::test]
    async fn fetches_grok_product_quota_and_identity_read_only() {
        use wiremock::matchers::{bearer_token, header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/billing"))
            .and(query_param("format", "credits"))
            .and(bearer_token("access"))
            .and(header("x-grok-client-mode", "cli"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "config": {
                    "creditUsagePercent": 25,
                    "billingPeriodEnd": "2026-08-01T00:00:00Z",
                    "productUsage": [
                        {"product": "grok_code", "usagePercent": 40},
                        {"product": "grok_web", "usagePercent": 10}
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(query_param("include", "subscription"))
            .and(bearer_token("access"))
            .and(header("x-grok-client-mode", "cli"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "email": "user@example.com",
                "firstName": "Example",
                "subscriptionTier": "supergrok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let snapshot = fetch_grok_quota_from(&reqwest::Client::new(), "access", &server.uri())
            .await
            .unwrap();

        assert_eq!(snapshot.account_label, "Grok · SuperGrok");
        assert_eq!(snapshot.detail.as_deref(), Some("Example"));
        assert_eq!(snapshot.buckets.len(), 3);
        assert_eq!(snapshot.buckets[0].label, "Credits");
        assert_eq!(snapshot.buckets[0].remaining, Some(0.75));
        assert_eq!(snapshot.buckets[1].label, "Grok code");
        assert_eq!(snapshot.buckets[1].remaining, Some(0.6));
        assert_eq!(snapshot.buckets[2].remaining, Some(0.9));
    }

    #[tokio::test]
    async fn fetches_cursor_plan_quota_from_app_session_cookie() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usage-summary"))
            .and(header(
                "cookie",
                "WorkosCursorSessionToken=user%3A%3Aaccess",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "billingCycleEnd": "2026-08-01T00:00:00Z",
                "membershipType": "pro",
                "individualUsage": {"plan": {
                    "used": 2500,
                    "limit": 10000,
                    "autoPercentUsed": 20,
                    "apiPercentUsed": 30,
                    "totalPercentUsed": 25
                }}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/me"))
            .and(header(
                "cookie",
                "WorkosCursorSessionToken=user%3A%3Aaccess",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "email": "user@example.com",
                "name": "Example User"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let snapshot = fetch_cursor_quota_from(
            &reqwest::Client::new(),
            "WorkosCursorSessionToken=user%3A%3Aaccess",
            &format!("{}/usage-summary", server.uri()),
            &format!("{}/me", server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(snapshot.account_label, "Cursor · Pro");
        assert_eq!(snapshot.detail.as_deref(), Some("Example User"));
        assert_eq!(snapshot.buckets.len(), 3);
        assert_eq!(snapshot.buckets[0].label, "Billing cycle");
        assert_eq!(snapshot.buckets[0].remaining, Some(0.75));
        assert_eq!(snapshot.buckets[1].remaining, Some(0.8));
        assert_eq!(snapshot.buckets[2].remaining, Some(0.7));
    }

    #[test]
    fn derives_cursor_cookie_only_from_valid_safe_subject() {
        let access = jwt(json!({"exp": 2_000, "sub": "google-oauth2|user_123"}));
        let session = cursor_app_session(&access, UNIX_EPOCH + Duration::from_secs(1_000)).unwrap();
        assert_eq!(
            session.cookie,
            format!("WorkosCursorSessionToken=user_123%3A%3A{access}")
        );

        let unsafe_subject = jwt(json!({"exp": 2_000, "sub": "provider|bad user"}));
        assert!(
            cursor_app_session(&unsafe_subject, UNIX_EPOCH + Duration::from_secs(1_000)).is_none()
        );
        let expired = jwt(json!({"exp": 500, "sub": "provider|user"}));
        assert!(cursor_app_session(&expired, UNIX_EPOCH + Duration::from_secs(1_000)).is_none());
    }

    #[test]
    fn rejects_codex_api_key_mode() {
        assert!(parse_codex(
            &json!({"auth_mode": "ApiKey", "OPENAI_API_KEY": "secret"}),
            SystemTime::now()
        )
        .is_none());
    }
}
