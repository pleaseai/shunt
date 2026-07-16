use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::admin::session::{random_id, RateLimiter};

use super::approval::Identity;

pub const DEVICE_CODE_TTL: Duration = Duration::from_secs(600);
pub const INITIAL_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

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
pub struct DeviceGrantStore {
    grants: Mutex<HashMap<String, DeviceGrant>>,
    by_user_code: Mutex<HashMap<String, String>>,
}

impl DeviceGrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, device_code: String, user_code: String) {
        self.create_at(device_code, user_code, Instant::now(), DEVICE_CODE_TTL);
    }

    fn create_at(&self, device_code: String, user_code: String, now: Instant, ttl: Duration) {
        self.by_user_code
            .lock()
            .expect("gateway user-code lock poisoned")
            .insert(user_code.clone(), device_code.clone());
        self.grants
            .lock()
            .expect("gateway device-grant lock poisoned")
            .insert(
                device_code,
                DeviceGrant {
                    user_code,
                    status: DeviceStatus::Pending,
                    expires: now + ttl,
                    next_poll: None,
                    poll_interval: INITIAL_POLL_INTERVAL,
                },
            );
    }

    pub fn user_code_available(&self, user_code: &str) -> bool {
        !self
            .by_user_code
            .lock()
            .expect("gateway user-code lock poisoned")
            .contains_key(user_code)
    }

    pub fn approve(&self, user_code: &str, identity: Identity) -> bool {
        self.set_status(user_code, DeviceStatus::Approved(identity))
    }

    pub fn deny(&self, user_code: &str) -> bool {
        self.set_status(user_code, DeviceStatus::Denied)
    }

    fn set_status(&self, user_code: &str, status: DeviceStatus) -> bool {
        let device_code = self
            .by_user_code
            .lock()
            .expect("gateway user-code lock poisoned")
            .get(user_code)
            .cloned();
        let Some(device_code) = device_code else {
            return false;
        };
        let mut grants = self
            .grants
            .lock()
            .expect("gateway device-grant lock poisoned");
        let Some(grant) = grants.get_mut(&device_code) else {
            return false;
        };
        if grant.expires <= Instant::now() || grant.status != DeviceStatus::Pending {
            return false;
        }
        grant.status = status;
        true
    }

    pub fn poll(&self, device_code: &str) -> DevicePoll {
        self.poll_at(device_code, Instant::now())
    }

    fn poll_at(&self, device_code: &str, now: Instant) -> DevicePoll {
        let mut grants = self
            .grants
            .lock()
            .expect("gateway device-grant lock poisoned");
        let Some(grant) = grants.get_mut(device_code) else {
            return DevicePoll::Expired;
        };
        if grant.expires <= now {
            let user_code = grant.user_code.clone();
            grants.remove(device_code);
            self.by_user_code
                .lock()
                .expect("gateway user-code lock poisoned")
                .remove(&user_code);
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
            DeviceStatus::Approved(identity) => DevicePoll::Approved(identity.clone()),
        }
    }

    pub fn consume(&self, device_code: &str) {
        let grant = self
            .grants
            .lock()
            .expect("gateway device-grant lock poisoned")
            .remove(device_code);
        if let Some(grant) = grant {
            self.by_user_code
                .lock()
                .expect("gateway user-code lock poisoned")
                .remove(&grant.user_code);
        }
    }
}

#[derive(Clone)]
struct RefreshEntry {
    identity: Identity,
    family: String,
    active: bool,
}

#[derive(Default)]
pub struct RefreshTokenStore {
    tokens: Mutex<HashMap<String, RefreshEntry>>,
}

impl RefreshTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(&self, identity: Identity) -> String {
        let token = random_id();
        let family = random_id();
        self.tokens
            .lock()
            .expect("gateway refresh-token lock poisoned")
            .insert(
                token.clone(),
                RefreshEntry {
                    identity,
                    family,
                    active: true,
                },
            );
        token
    }

    pub fn rotate(&self, presented: &str) -> Option<(Identity, String)> {
        let mut tokens = self
            .tokens
            .lock()
            .expect("gateway refresh-token lock poisoned");
        let entry = tokens.get(presented)?.clone();
        if !entry.active {
            revoke_family(&mut tokens, &entry.family);
            return None;
        }
        if let Some(old) = tokens.get_mut(presented) {
            old.active = false;
        }
        let next = random_id();
        tokens.insert(
            next.clone(),
            RefreshEntry {
                identity: entry.identity.clone(),
                family: entry.family,
                active: true,
            },
        );
        Some((entry.identity, next))
    }
}

fn revoke_family(tokens: &mut HashMap<String, RefreshEntry>, family: &str) {
    for entry in tokens.values_mut().filter(|entry| entry.family == family) {
        entry.active = false;
    }
}

pub struct PerIpRateLimiter {
    limits: Mutex<HashMap<String, RateLimiter>>,
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
        self.limits
            .lock()
            .expect("gateway rate-limit lock poisoned")
            .entry(ip.to_string())
            .or_insert_with(|| RateLimiter::new(self.window, self.max))
            .check()
    }
}

pub struct GatewayStores {
    pub device_grants: DeviceGrantStore,
    pub refresh_tokens: RefreshTokenStore,
    pub device_verify_rate: PerIpRateLimiter,
}

impl GatewayStores {
    pub fn new() -> Self {
        Self {
            device_grants: DeviceGrantStore::new(),
            refresh_tokens: RefreshTokenStore::new(),
            device_verify_rate: PerIpRateLimiter::new(Duration::from_secs(60), 30),
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

    #[test]
    fn device_grant_transitions_and_is_single_use() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        store.create_at("device".into(), "BCDF-GHJK".into(), now, DEVICE_CODE_TTL);

        assert_eq!(store.poll_at("device", now), DevicePoll::Pending);
        assert!(store.approve("BCDF-GHJK", identity()));
        assert_eq!(
            store.poll_at("device", now + INITIAL_POLL_INTERVAL),
            DevicePoll::Approved(identity())
        );
        store.consume("device");
        assert_eq!(store.poll_at("device", now), DevicePoll::Expired);
        assert!(!store.approve("BCDF-GHJK", identity()));
    }

    #[test]
    fn device_grant_denies_and_expires() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        store.create_at("denied".into(), "BCDF-GHJL".into(), now, DEVICE_CODE_TTL);
        assert!(store.deny("BCDF-GHJL"));
        assert_eq!(store.poll_at("denied", now), DevicePoll::Denied);

        store.create_at("expired".into(), "BCDF-GHJM".into(), now, Duration::ZERO);
        assert_eq!(store.poll_at("expired", now), DevicePoll::Expired);
    }

    #[test]
    fn fast_poll_adds_five_seconds_to_interval() {
        let store = DeviceGrantStore::new();
        let now = Instant::now();
        store.create_at("device".into(), "BCDF-GHJK".into(), now, DEVICE_CODE_TTL);

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
    fn refresh_rotates_and_reuse_revokes_the_family() {
        let store = RefreshTokenStore::new();
        let first = store.issue(identity());
        let (rotated_identity, second) = store.rotate(&first).expect("first rotation");
        assert_eq!(rotated_identity, identity());
        assert_ne!(first, second);

        assert!(store.rotate(&first).is_none(), "old token is single-use");
        assert!(
            store.rotate(&second).is_none(),
            "reuse detection revokes the newly rotated token"
        );
        assert!(store.rotate("unknown").is_none());
    }

    #[test]
    fn rate_limit_is_scoped_by_ip() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        assert!(limiter.check("192.0.2.1"));
        assert!(!limiter.check("192.0.2.1"));
        assert!(limiter.check("192.0.2.2"));
    }
}
