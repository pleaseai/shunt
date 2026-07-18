//! Process-lifetime stores for OAuth device grants and browser verification
//! limits, plus the [`GatewayStores`] bundle that also carries the refresh
//! token store ([`super::refresh`]).
//!
//! Mutating operations opportunistically sweep expired or idle state. Device
//! grants and rate-limit counters are deliberately memory-only: they are
//! short-lived, so a restart only costs an in-flight login attempt.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::{
    approval::Identity, idp_client::DiscoveredEndpoints, refresh::RefreshTokenStore, ResolvedIdp,
};

pub const DEVICE_CODE_TTL: Duration = Duration::from_secs(600);
pub const OIDC_STATE_TTL: Duration = Duration::from_secs(600);
pub const INITIAL_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
const MAX_DEVICE_GRANTS: usize = 4096;
const MAX_OIDC_STATES: usize = 4096;
const MAX_RATE_LIMIT_IDENTITIES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceStatus {
    Pending,
    Approved(Identity),
    Denied,
}

struct DeviceGrant {
    user_code: String,
    status: DeviceStatus,
    expires: Instant,
    next_poll: Option<Instant>,
    poll_interval: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Approved(Identity),
}

#[derive(Default)]
struct DeviceGrantState {
    grants: HashMap<String, DeviceGrant>,
    by_user_code: HashMap<String, String>,
}

#[derive(Default)]
pub struct DeviceGrantStore {
    state: Mutex<DeviceGrantState>,
}

impl DeviceGrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, device_code: String, user_code: String) -> bool {
        self.create_at(device_code, user_code, Instant::now(), DEVICE_CODE_TTL)
    }

    fn create_at(
        &self,
        device_code: String,
        user_code: String,
        now: Instant,
        ttl: Duration,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("gateway device-grant lock poisoned");
        sweep_expired_grants(&mut state, now);
        if state.by_user_code.contains_key(&user_code)
            || state.grants.contains_key(&device_code)
            || state.grants.len() >= MAX_DEVICE_GRANTS
        {
            return false;
        }
        state
            .by_user_code
            .insert(user_code.clone(), device_code.clone());
        state.grants.insert(
            device_code,
            DeviceGrant {
                user_code,
                status: DeviceStatus::Pending,
                expires: now + ttl,
                next_poll: None,
                poll_interval: INITIAL_POLL_INTERVAL,
            },
        );
        true
    }

    pub fn approve(&self, user_code: &str, identity: Identity) -> bool {
        self.set_status_at(user_code, DeviceStatus::Approved(identity), Instant::now())
    }

    pub fn pending_exists(&self, user_code: &str) -> bool {
        self.pending_exists_at(user_code, Instant::now())
    }

    fn pending_exists_at(&self, user_code: &str, now: Instant) -> bool {
        let state = self
            .state
            .lock()
            .expect("gateway device-grant lock poisoned");
        state
            .by_user_code
            .get(user_code)
            .and_then(|device_code| state.grants.get(device_code))
            .is_some_and(|grant| grant.expires > now && grant.status == DeviceStatus::Pending)
    }

    pub fn deny(&self, user_code: &str) -> bool {
        self.set_status_at(user_code, DeviceStatus::Denied, Instant::now())
    }

    fn set_status_at(&self, user_code: &str, status: DeviceStatus, now: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("gateway device-grant lock poisoned");
        let Some(device_code) = state.by_user_code.get(user_code).cloned() else {
            return false;
        };
        let Some(grant) = state.grants.get_mut(&device_code) else {
            return false;
        };
        if grant.expires <= now || grant.status != DeviceStatus::Pending {
            return false;
        }
        grant.status = status;
        true
    }

    pub fn poll(&self, device_code: &str) -> DevicePoll {
        self.poll_at(device_code, Instant::now())
    }

    fn poll_at(&self, device_code: &str, now: Instant) -> DevicePoll {
        let mut state = self
            .state
            .lock()
            .expect("gateway device-grant lock poisoned");
        let Some(grant) = state.grants.get_mut(device_code) else {
            return DevicePoll::Expired;
        };
        if grant.expires <= now {
            let user_code = grant.user_code.clone();
            state.grants.remove(device_code);
            state.by_user_code.remove(&user_code);
            return DevicePoll::Expired;
        }
        if grant.next_poll.is_some_and(|next| now < next) {
            grant.poll_interval += SLOW_DOWN_INCREMENT;
            grant.next_poll = Some(now + grant.poll_interval);
            return DevicePoll::SlowDown;
        }
        grant.next_poll = Some(now + grant.poll_interval);
        match &grant.status {
            DeviceStatus::Pending => DevicePoll::Pending,
            DeviceStatus::Denied => DevicePoll::Denied,
            DeviceStatus::Approved(identity) => {
                let identity = identity.clone();
                let user_code = grant.user_code.clone();
                state.grants.remove(device_code);
                state.by_user_code.remove(&user_code);
                DevicePoll::Approved(identity)
            }
        }
    }
}

fn sweep_expired_grants(state: &mut DeviceGrantState, now: Instant) {
    let expired_user_codes: Vec<_> = state
        .grants
        .values()
        .filter(|grant| grant.expires <= now)
        .map(|grant| grant.user_code.clone())
        .collect();
    state.grants.retain(|_, grant| grant.expires > now);
    for user_code in expired_user_codes {
        state.by_user_code.remove(&user_code);
    }
}

#[derive(Clone)]
pub struct OidcPending {
    pub user_code: String,
    pub verifier: String,
    pub idp: Arc<ResolvedIdp>,
    pub redirect_uri: String,
    expires: Instant,
}

#[derive(Default)]
pub struct OidcStateStore {
    states: Mutex<HashMap<String, OidcPending>>,
}

impl OidcStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &self,
        state: String,
        user_code: String,
        verifier: String,
        idp: Arc<ResolvedIdp>,
        redirect_uri: String,
    ) -> bool {
        self.insert_at(
            state,
            user_code,
            verifier,
            idp,
            redirect_uri,
            Instant::now(),
            OIDC_STATE_TTL,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_at(
        &self,
        state: String,
        user_code: String,
        verifier: String,
        idp: Arc<ResolvedIdp>,
        redirect_uri: String,
        now: Instant,
        ttl: Duration,
    ) -> bool {
        let mut states = self
            .states
            .lock()
            .expect("gateway OIDC-state lock poisoned");
        states.retain(|_, pending| pending.expires > now);
        if states.contains_key(&state) {
            return false;
        }
        // Keep exactly one active browser authorization per device grant. A
        // second click replaces the old state so an abandoned tab cannot later
        // deny or approve the same grant independently.
        states.retain(|_, pending| pending.user_code != user_code);
        if states.len() >= MAX_OIDC_STATES {
            return false;
        }
        states.insert(
            state,
            OidcPending {
                user_code,
                verifier,
                idp,
                redirect_uri,
                expires: now + ttl,
            },
        );
        true
    }

    pub fn take(&self, state: &str) -> Option<OidcPending> {
        self.take_at(state, Instant::now())
    }

    fn take_at(&self, state: &str, now: Instant) -> Option<OidcPending> {
        self.states
            .lock()
            .expect("gateway OIDC-state lock poisoned")
            .remove(state)
            .filter(|pending| pending.expires > now)
    }
}

struct RateLimitEntry {
    window_start: Instant,
    count: u32,
    last_seen: Instant,
}

pub struct PerIpRateLimiter {
    limits: Mutex<HashMap<String, RateLimitEntry>>,
    window: Duration,
    max: u32,
}

impl PerIpRateLimiter {
    pub fn new(window: Duration, max: u32) -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            window,
            max,
        }
    }

    pub fn check(&self, ip: &str) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: &str, now: Instant) -> bool {
        let mut limits = self
            .limits
            .lock()
            .expect("gateway rate-limit lock poisoned");
        limits.retain(|_, entry| now.saturating_duration_since(entry.last_seen) < self.window);
        if !limits.contains_key(ip) && limits.len() >= MAX_RATE_LIMIT_IDENTITIES {
            return false;
        }
        let entry = limits.entry(ip.to_string()).or_insert(RateLimitEntry {
            window_start: now,
            count: 0,
            last_seen: now,
        });
        if now.saturating_duration_since(entry.window_start) >= self.window {
            entry.window_start = now;
            entry.count = 0;
        }
        entry.last_seen = now;
        entry.count += 1;
        entry.count <= self.max
    }
}

pub struct GatewayStores {
    pub device_grants: DeviceGrantStore,
    pub refresh_tokens: RefreshTokenStore,
    pub device_verify_rate: PerIpRateLimiter,
    pub oidc_states: OidcStateStore,
    pub oidc_discovery: Mutex<HashMap<String, DiscoveredEndpoints>>,
    pub oidc_client: reqwest::Client,
}

impl GatewayStores {
    pub fn new() -> Self {
        Self {
            device_grants: DeviceGrantStore::new(),
            refresh_tokens: RefreshTokenStore::new(),
            device_verify_rate: PerIpRateLimiter::new(Duration::from_secs(60), 30),
            oidc_states: OidcStateStore::new(),
            oidc_discovery: Mutex::new(HashMap::new()),
            oidc_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("OIDC HTTP client configuration is valid"),
        }
    }
}

impl Default for GatewayStores {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            sub: "dev@example.com".into(),
            email: "dev@example.com".into(),
            name: "dev".into(),
        }
    }

    fn idp() -> Arc<ResolvedIdp> {
        Arc::new(ResolvedIdp {
            issuer: "https://idp.example".into(),
            client_id: "client".into(),
            client_secret: "secret".into(),
            allowed_domains: vec!["example.com".into()],
            allowed_emails: vec![],
            scopes: vec!["openid".into()],
            authorization_endpoint: None,
            token_endpoint: None,
            userinfo_endpoint: None,
        })
    }

    #[test]
    fn device_grant_transitions_and_is_single_use() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        assert!(store.create_at("device".into(), "BCDF-GHJK".into(), now, DEVICE_CODE_TTL));
        assert!(store.pending_exists_at("BCDF-GHJK", now));
        assert!(!store.create_at(
            "other-device".into(),
            "BCDF-GHJK".into(),
            now,
            DEVICE_CODE_TTL
        ));

        assert_eq!(store.poll_at("device", now), DevicePoll::Pending);
        assert!(store.approve("BCDF-GHJK", identity()));
        assert!(!store.pending_exists_at("BCDF-GHJK", now));
        assert_eq!(
            store.poll_at("device", now + INITIAL_POLL_INTERVAL),
            DevicePoll::Approved(identity())
        );
        assert_eq!(store.poll_at("device", now), DevicePoll::Expired);
        assert!(!store.approve("BCDF-GHJK", identity()));
    }

    #[test]
    fn device_grant_denies_and_expires() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        assert!(store.create_at("denied".into(), "BCDF-GHJL".into(), now, DEVICE_CODE_TTL));
        assert!(store.deny("BCDF-GHJL"));
        assert_eq!(store.poll_at("denied", now), DevicePoll::Denied);

        assert!(store.create_at("expired".into(), "BCDF-GHJM".into(), now, Duration::ZERO));
        assert!(!store.set_status_at("BCDF-GHJM", DeviceStatus::Approved(identity()), now,));
    }

    #[test]
    fn fast_poll_adds_five_seconds_to_interval() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        assert!(store.create_at("device".into(), "BCDF-GHJK".into(), now, DEVICE_CODE_TTL));

        assert_eq!(store.poll_at("device", now), DevicePoll::Pending);
        assert_eq!(
            store.poll_at("device", now + Duration::from_secs(4)),
            DevicePoll::SlowDown
        );
        assert_eq!(
            store.poll_at("device", now + Duration::from_secs(10)),
            DevicePoll::SlowDown,
            "the slow-down response extends the next allowed interval to ten seconds"
        );
        assert_eq!(
            store.poll_at("device", now + Duration::from_secs(19)),
            DevicePoll::SlowDown
        );
    }

    #[test]
    fn creating_a_grant_sweeps_abandoned_expired_grants() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        assert!(store.create_at("expired".into(), "BCDF-GHJK".into(), now, Duration::ZERO));
        assert!(store.create_at(
            "current".into(),
            "BCDF-GHJL".into(),
            now + Duration::from_secs(1),
            DEVICE_CODE_TTL
        ));

        let state = store.state.lock().unwrap();
        assert_eq!(state.grants.len(), 1);
        assert_eq!(state.by_user_code.len(), 1);
        assert!(state.grants.contains_key("current"));
        assert!(!state.by_user_code.contains_key("BCDF-GHJK"));
    }

    #[test]
    fn device_grants_reject_admission_at_capacity() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        for index in 0..MAX_DEVICE_GRANTS {
            assert!(store.create_at(
                format!("device-{index}"),
                format!("CODE-{index}"),
                now,
                DEVICE_CODE_TTL,
            ));
        }
        assert!(!store.create_at("overflow".into(), "OVER-FLOW".into(), now, DEVICE_CODE_TTL,));
    }

    #[test]
    fn oidc_states_are_single_use_and_expire() {
        let store = OidcStateStore::new();
        let now = Instant::now();
        assert!(store.insert_at(
            "state".into(),
            "BCDF-GHJK".into(),
            "verifier".into(),
            idp(),
            "https://gateway.example/device/callback".into(),
            now,
            OIDC_STATE_TTL,
        ));
        assert!(store.insert_at(
            "replacement".into(),
            "BCDF-GHJK".into(),
            "new-verifier".into(),
            idp(),
            "https://gateway.example/device/callback".into(),
            now,
            OIDC_STATE_TTL,
        ));
        assert!(store.take_at("state", now).is_none());
        let pending = store
            .take_at("replacement", now)
            .expect("replacement state exists");
        assert_eq!(pending.verifier, "new-verifier");

        assert!(store.insert_at(
            "expired".into(),
            "BCDF-GHJL".into(),
            "verifier".into(),
            idp(),
            "https://gateway.example/device/callback".into(),
            now,
            Duration::ZERO,
        ));
        assert!(store.take_at("expired", now).is_none());
    }

    #[test]
    fn oidc_states_reject_admission_at_capacity() {
        let store = OidcStateStore::new();
        let now = Instant::now();
        for index in 0..MAX_OIDC_STATES {
            assert!(store.insert_at(
                format!("state-{index}"),
                format!("CODE-{index}"),
                format!("verifier-{index}"),
                idp(),
                "https://gateway.example/device/callback".into(),
                now,
                OIDC_STATE_TTL,
            ));
        }
        assert!(!store.insert_at(
            "overflow".into(),
            "OVER-FLOW".into(),
            "verifier".into(),
            idp(),
            "https://gateway.example/device/callback".into(),
            now,
            OIDC_STATE_TTL,
        ));
    }

    #[test]
    fn rate_limit_rejects_new_identity_at_capacity() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        for index in 0..MAX_RATE_LIMIT_IDENTITIES {
            assert!(limiter.check_at(&format!("client-{index}"), now));
        }
        assert!(!limiter.check_at("overflow", now));
        assert!(!limiter.check_at("client-0", now));
    }

    #[test]
    fn rate_limit_sweep_evicts_idle_ips() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(limiter.check_at("192.0.2.1", now));
        assert!(limiter.check_at("192.0.2.2", now));
        assert_eq!(limiter.limits.lock().unwrap().len(), 2);

        assert!(limiter.check_at("192.0.2.3", now + Duration::from_secs(60)));
        let limits = limiter.limits.lock().unwrap();
        assert_eq!(limits.len(), 1);
        assert!(limits.contains_key("192.0.2.3"));
    }

    #[test]
    fn rate_limit_is_scoped_by_ip() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        assert!(limiter.check("192.0.2.1"));
        assert!(!limiter.check("192.0.2.1"));
        assert!(limiter.check("192.0.2.2"));
    }
}
