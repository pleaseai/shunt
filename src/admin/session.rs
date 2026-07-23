//! In-memory, process-lifetime stores backing the admin web surface (M9).
//!
//! These survive config hot reloads (they live in `AppState`, not the reloaded
//! `RuntimeState`), so an operator's browser session is not dropped by an
//! unrelated config edit. Everything here is single-process and single-use where
//! it matters; nothing is persisted to disk. The [`PendingStore`] is deliberately
//! generic so the planned gateway-login device flow can reuse it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A fresh 256-bit URL-safe random identifier (session id or CSRF token).
pub(crate) fn random_id() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Maximum completion attempts per pending login before it is discarded, so a
/// wrong paste can be retried a few times but code/state guessing cannot run
/// indefinitely (the 256-bit `state` already makes guessing infeasible).
const MAX_PENDING_ATTEMPTS: u32 = 5;
const MAX_OIDC_STATES: usize = 4096;
const OBSERVED_USAGE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct CachedObservedUsage {
    observed_at: Instant,
    token_hash: [u8; 32],
    snapshot: crate::accounts::UsageSnapshot,
}

/// Token-free cache for read-only provider usage observation. Browser refreshes
/// inside the TTL reuse the last snapshot instead of polling the provider again.
pub struct ObservedUsageCache {
    claude: Mutex<Option<CachedObservedUsage>>,
}

impl ObservedUsageCache {
    fn new() -> Self {
        Self {
            claude: Mutex::new(None),
        }
    }

    pub fn claude(&self, access_token: &str) -> Option<crate::accounts::UsageSnapshot> {
        self.claude_at(access_token, Instant::now())
    }

    pub fn claude_at(
        &self,
        access_token: &str,
        now: Instant,
    ) -> Option<crate::accounts::UsageSnapshot> {
        let token_hash: [u8; 32] = Sha256::digest(access_token.as_bytes()).into();
        self.claude
            .lock()
            .expect("observed usage cache lock poisoned")
            .as_ref()
            .filter(|cached| {
                cached.token_hash == token_hash
                    && now.duration_since(cached.observed_at) < OBSERVED_USAGE_TTL
            })
            .map(|cached| cached.snapshot.clone())
    }

    pub fn store_claude(&self, access_token: &str, snapshot: crate::accounts::UsageSnapshot) {
        self.store_claude_at(access_token, snapshot, Instant::now());
    }

    pub fn store_claude_at(
        &self,
        access_token: &str,
        snapshot: crate::accounts::UsageSnapshot,
        now: Instant,
    ) {
        *self
            .claude
            .lock()
            .expect("observed usage cache lock poisoned") = Some(CachedObservedUsage {
            observed_at: now,
            token_hash: Sha256::digest(access_token.as_bytes()).into(),
            snapshot,
        });
    }
}

struct Session {
    csrf: String,
    expires: Instant,
}

/// Authenticated admin browser sessions, keyed by an opaque session id carried in
/// an `HttpOnly` cookie. Each session carries a CSRF token bound to it.
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session with the given lifetime. Returns `(session_id, csrf)`.
    pub fn create(&self, ttl: Duration) -> (String, String) {
        let id = random_id();
        let csrf = random_id();
        self.sessions
            .lock()
            .expect("admin session lock poisoned")
            .insert(
                id.clone(),
                Session {
                    csrf: csrf.clone(),
                    expires: Instant::now() + ttl,
                },
            );
        (id, csrf)
    }

    /// The session's CSRF token if the id is valid and unexpired. Prunes the
    /// entry when it has expired.
    pub fn csrf_for(&self, id: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().expect("admin session lock poisoned");
        match sessions.get(id) {
            Some(session) if session.expires > Instant::now() => Some(session.csrf.clone()),
            Some(_) => {
                sessions.remove(id);
                None
            }
            None => None,
        }
    }

    pub fn remove(&self, id: &str) {
        self.sessions
            .lock()
            .expect("admin session lock poisoned")
            .remove(id);
    }
}

/// The upstream credential flow associated with a pending login. Stored alongside
/// the PKCE secrets so completion cannot be switched to a different token shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingKind {
    SetupToken,
    FullOauth,
    CodexOauth,
}

/// The secrets and credential kind needed to complete a pending provider login.
#[derive(Clone)]
pub struct PendingLogin {
    pub kind: PendingKind,
    pub verifier: String,
    pub state: String,
}

struct PendingEntry {
    kind: PendingKind,
    verifier: String,
    state: String,
    expires: Instant,
    attempts: u32,
}

/// Outcome of registering a completion attempt against a pending login.
pub enum PendingAttempt {
    /// The pending login exists, is unexpired, and is under the attempt cap.
    Ready(PendingLogin),
    /// No pending login (missing, expired, or already consumed).
    NotFound,
    /// The attempt cap was reached; the entry has been discarded.
    TooManyAttempts,
}

/// Short-lived, single-use pending logins keyed by account name. A completion
/// consumes the entry on success; wrong pastes are capped by [`MAX_PENDING_ATTEMPTS`].
#[derive(Default)]
pub struct PendingStore {
    pending: Mutex<HashMap<String, PendingEntry>>,
}

impl PendingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start (or replace) a pending login for `key` with the given lifetime.
    pub fn start(
        &self,
        key: &str,
        kind: PendingKind,
        verifier: String,
        state: String,
        ttl: Duration,
    ) {
        self.pending
            .lock()
            .expect("admin pending lock poisoned")
            .insert(
                key.to_string(),
                PendingEntry {
                    kind,
                    verifier,
                    state,
                    expires: Instant::now() + ttl,
                    attempts: 0,
                },
            );
    }

    /// Register a completion attempt. Returns the pending secrets when the entry
    /// is present, unexpired, and under the attempt cap; discards the entry once
    /// the cap is reached so a brute-force sequence cannot continue.
    pub fn attempt(&self, key: &str) -> PendingAttempt {
        let mut pending = self.pending.lock().expect("admin pending lock poisoned");
        let Some(entry) = pending.get_mut(key) else {
            return PendingAttempt::NotFound;
        };
        if entry.expires <= Instant::now() {
            pending.remove(key);
            return PendingAttempt::NotFound;
        }
        entry.attempts += 1;
        if entry.attempts > MAX_PENDING_ATTEMPTS {
            pending.remove(key);
            return PendingAttempt::TooManyAttempts;
        }
        PendingAttempt::Ready(PendingLogin {
            kind: entry.kind,
            verifier: entry.verifier.clone(),
            state: entry.state.clone(),
        })
    }

    /// Remove a pending login after a successful completion (single-use).
    pub fn remove(&self, key: &str) {
        self.pending
            .lock()
            .expect("admin pending lock poisoned")
            .remove(key);
    }
}

/// The PKCE snapshot needed to complete one admin OIDC browser login.
pub struct OidcPending {
    pub verifier: String,
    pub idp: Arc<crate::gateway::ResolvedIdp>,
    pub redirect_uri: String,
    expires: Instant,
}

/// Short-lived, single-use admin OIDC states keyed by the PKCE state value.
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
        verifier: String,
        idp: Arc<crate::gateway::ResolvedIdp>,
        redirect_uri: String,
        ttl: Duration,
    ) -> bool {
        self.insert_at(state, verifier, idp, redirect_uri, Instant::now(), ttl)
    }

    fn insert_at(
        &self,
        state: String,
        verifier: String,
        idp: Arc<crate::gateway::ResolvedIdp>,
        redirect_uri: String,
        now: Instant,
        ttl: Duration,
    ) -> bool {
        let mut states = self.states.lock().expect("admin OIDC-state lock poisoned");
        states.retain(|_, pending| pending.expires > now);
        if states.contains_key(&state) || states.len() >= MAX_OIDC_STATES {
            return false;
        }
        states.insert(
            state,
            OidcPending {
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
            .expect("admin OIDC-state lock poisoned")
            .remove(state)
            .filter(|pending| pending.expires > now)
    }
}

struct Window {
    start: Instant,
    count: u32,
}

/// Coarse fixed-window rate limiter guarding the completion endpoint against
/// code-guessing storms across all pending logins.
pub struct RateLimiter {
    window: Duration,
    max: u32,
    state: Mutex<Window>,
}

impl RateLimiter {
    pub fn new(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            state: Mutex::new(Window {
                start: Instant::now(),
                count: 0,
            }),
        }
    }

    /// Record a call; `true` if within budget, `false` if the window is exhausted.
    pub fn check(&self) -> bool {
        let mut state = self.state.lock().expect("admin rate-limit lock poisoned");
        let now = Instant::now();
        if now.duration_since(state.start) >= self.window {
            state.start = now;
            state.count = 0;
        }
        state.count += 1;
        state.count <= self.max
    }
}

/// Process-lifetime stores for the admin surface, created once in `build_router`.
pub struct AdminStores {
    pub sessions: SessionStore,
    pub pending: PendingStore,
    pub oidc_states: OidcStateStore,
    pub observed_usage: ObservedUsageCache,
    /// Guards the completion endpoint against code-guessing storms.
    pub complete_rate: RateLimiter,
    /// Guards the login endpoint against admin-token brute-force. Coarse and
    /// process-global (like `complete_rate`): throttles guessing throughput as
    /// defense-in-depth behind the constant-time token compare.
    pub login_rate: RateLimiter,
}

impl AdminStores {
    pub fn new() -> Self {
        Self {
            sessions: SessionStore::new(),
            pending: PendingStore::new(),
            oidc_states: OidcStateStore::new(),
            observed_usage: ObservedUsageCache::new(),
            complete_rate: RateLimiter::new(Duration::from_secs(60), 30),
            login_rate: RateLimiter::new(Duration::from_secs(60), 30),
        }
    }
}

impl Default for AdminStores {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idp() -> Arc<crate::gateway::ResolvedIdp> {
        Arc::new(crate::gateway::ResolvedIdp {
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
    fn session_round_trips_and_expires() {
        let store = SessionStore::new();
        let (id, csrf) = store.create(Duration::from_secs(60));
        assert_eq!(store.csrf_for(&id).as_deref(), Some(csrf.as_str()));
        assert!(store.csrf_for("nope").is_none());

        let (expired, _) = store.create(Duration::from_millis(0));
        // A zero (already-elapsed) TTL is treated as expired and pruned.
        assert!(store.csrf_for(&expired).is_none());

        store.remove(&id);
        assert!(store.csrf_for(&id).is_none());
    }

    #[test]
    fn pending_is_single_use_and_attempt_capped() {
        let store = PendingStore::new();
        store.start(
            "main",
            PendingKind::SetupToken,
            "verifier".into(),
            "state".into(),
            Duration::from_secs(60),
        );

        // First attempt returns the secrets and the flow kind; a successful
        // completion removes it.
        let PendingAttempt::Ready(pending) = store.attempt("main") else {
            panic!("expected pending login");
        };
        assert_eq!(pending.kind, PendingKind::SetupToken);
        store.remove("main");
        assert!(matches!(store.attempt("main"), PendingAttempt::NotFound));

        // Attempt cap: after MAX_PENDING_ATTEMPTS the entry is discarded.
        store.start(
            "cap",
            PendingKind::FullOauth,
            "v".into(),
            "s".into(),
            Duration::from_secs(60),
        );
        for _ in 0..MAX_PENDING_ATTEMPTS {
            assert!(matches!(store.attempt("cap"), PendingAttempt::Ready(_)));
        }
        assert!(matches!(
            store.attempt("cap"),
            PendingAttempt::TooManyAttempts
        ));
        assert!(matches!(store.attempt("cap"), PendingAttempt::NotFound));
    }

    #[test]
    fn pending_expires() {
        let store = PendingStore::new();
        store.start(
            "x",
            PendingKind::SetupToken,
            "v".into(),
            "s".into(),
            Duration::from_millis(0),
        );
        assert!(matches!(store.attempt("x"), PendingAttempt::NotFound));
    }

    #[test]
    fn oidc_states_are_single_use_and_expire() {
        let store = OidcStateStore::new();
        let now = Instant::now();
        assert!(store.insert_at(
            "state".into(),
            "verifier".into(),
            idp(),
            "https://admin.example/admin/oidc/callback".into(),
            now,
            Duration::from_secs(60),
        ));
        let pending = store.take_at("state", now).expect("state exists");
        assert_eq!(pending.verifier, "verifier");
        assert_eq!(
            pending.redirect_uri,
            "https://admin.example/admin/oidc/callback"
        );
        assert!(store.take_at("state", now).is_none());

        assert!(store.insert_at(
            "expired".into(),
            "verifier".into(),
            idp(),
            "https://admin.example/admin/oidc/callback".into(),
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
                format!("verifier-{index}"),
                idp(),
                "https://admin.example/admin/oidc/callback".into(),
                now,
                Duration::from_secs(60),
            ));
        }
        assert!(!store.insert_at(
            "overflow".into(),
            "verifier".into(),
            idp(),
            "https://admin.example/admin/oidc/callback".into(),
            now,
            Duration::from_secs(60),
        ));
    }

    #[test]
    fn observed_usage_cache_is_scoped_to_access_token() {
        let cache = ObservedUsageCache::new();
        let snapshot = crate::accounts::UsageSnapshot {
            five_hour: Some(crate::accounts::UsageWindow {
                utilization: 0.25,
                resets_at: None,
            }),
            ..Default::default()
        };
        let now = Instant::now();
        cache.store_claude_at("account-a-token", snapshot.clone(), now);

        assert_eq!(cache.claude_at("account-a-token", now), Some(snapshot));
        assert_eq!(cache.claude_at("account-b-token", now), None);
    }

    #[test]
    fn observed_usage_cache_expires_after_ttl() {
        let cache = ObservedUsageCache::new();
        let snapshot = crate::accounts::UsageSnapshot::default();
        let stored_at = Instant::now();
        cache.store_claude_at("token", snapshot.clone(), stored_at);

        assert_eq!(
            cache.claude_at(
                "token",
                stored_at + OBSERVED_USAGE_TTL - Duration::from_secs(1)
            ),
            Some(snapshot.clone())
        );
        assert_eq!(
            cache.claude_at("token", stored_at + OBSERVED_USAGE_TTL),
            None
        );
    }

    #[test]
    fn rate_limiter_caps_within_window() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 3);
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(!limiter.check());
    }
}
