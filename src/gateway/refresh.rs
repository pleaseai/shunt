//! Rotating opaque refresh tokens for the gateway login surface.
//!
//! Mutating operations opportunistically sweep expired or idle state. Used
//! refresh-token tombstones are retained for 30 days, capped at 64 per family:
//! replay within that bounded horizon revokes the active family, while older
//! tokens still fail as `invalid_grant` without keeping unbounded history.
//! Active refresh tokens idle for 30 days are swept the same way, so a session
//! that stops refreshing eventually ends instead of accumulating forever.
//!
//! Tokens are held keyed by their SHA-256 (the opaque token itself is 256-bit
//! random, so an unsalted hash is preimage-resistant), which lets the optional
//! `[server.gateway] state_path` persistence (issue #194) write the store to
//! disk without ever storing a usable token at rest. Times are unix seconds so
//! records survive a restart, unlike the monotonic clocks used by the
//! process-lifetime device-grant and rate-limit stores.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard,
    },
};

use sha2::{Digest, Sha256};

use crate::admin::session::random_id;

use super::approval::Identity;

const REFRESH_TOMBSTONE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// Active refresh tokens unused for this long are swept: a client that has not
/// refreshed in 30 days must sign in again. Bounds store (and state-file)
/// growth from abandoned sessions.
const REFRESH_IDLE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const MAX_TOMBSTONES_PER_FAMILY: usize = 64;

/// Lowercase hex SHA-256 of an opaque refresh token — the only form the token
/// is held in memory or written to disk.
fn token_sha256(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone)]
struct RefreshEntry {
    identity: Identity,
    family: String,
    /// Unix seconds when this token was superseded or revoked; `None` ⇒ active.
    inactive_since: Option<u64>,
    /// Unix seconds when this token was issued (by login or rotation). Active
    /// tokens idle past [`REFRESH_IDLE_TTL_SECS`] are swept.
    issued_at: u64,
}

/// One refresh-token session record as exchanged with the persistence layer
/// ([`crate::gateway::persist`]). Holds the token's SHA-256, never the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshRecord {
    pub token_sha256: String,
    pub identity: Identity,
    pub family: String,
    pub inactive_since: Option<u64>,
    pub issued_at: u64,
}

#[derive(Default)]
pub struct RefreshTokenStore {
    /// Keyed by the SHA-256 hex of the opaque token, never the token itself.
    tokens: Mutex<HashMap<String, RefreshEntry>>,
    /// Set by mutations, cleared by [`Self::take_dirty`]; gates persistence so
    /// non-mutating polls never rewrite the state file.
    dirty: AtomicBool,
    /// Serializes the persistence layer's export-then-write cycles; see
    /// [`Self::persist_gate`].
    persist_gate: Mutex<()>,
}

impl RefreshTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(&self, identity: Identity) -> String {
        self.issue_at(identity, unix_now())
    }

    fn issue_at(&self, identity: Identity, now: u64) -> String {
        let token = random_id();
        let family = random_id();
        let mut tokens = self
            .tokens
            .lock()
            .expect("gateway refresh-token lock poisoned");
        sweep_refresh_tokens(&mut tokens, now);
        tokens.insert(
            token_sha256(&token),
            RefreshEntry {
                identity,
                family,
                inactive_since: None,
                issued_at: now,
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
        token
    }

    pub fn rotate(&self, presented: &str) -> Option<(Identity, String)> {
        self.rotate_at(presented, unix_now())
    }

    fn rotate_at(&self, presented: &str, now: u64) -> Option<(Identity, String)> {
        let presented_hash = token_sha256(presented);
        let mut tokens = self
            .tokens
            .lock()
            .expect("gateway refresh-token lock poisoned");
        sweep_refresh_tokens(&mut tokens, now);
        let entry = tokens.get(&presented_hash)?.clone();
        if entry.inactive_since.is_some() {
            revoke_family(&mut tokens, &entry.family, now);
            sweep_refresh_tokens(&mut tokens, now);
            self.dirty.store(true, Ordering::Relaxed);
            return None;
        }
        if let Some(old) = tokens.get_mut(&presented_hash) {
            old.inactive_since = Some(now);
        }
        let next = random_id();
        tokens.insert(
            token_sha256(&next),
            RefreshEntry {
                identity: entry.identity.clone(),
                family: entry.family,
                inactive_since: None,
                issued_at: now,
            },
        );
        sweep_refresh_tokens(&mut tokens, now);
        self.dirty.store(true, Ordering::Relaxed);
        Some((entry.identity, next))
    }

    /// Snapshot every live record (active tokens and tombstones) for
    /// persistence.
    pub fn export(&self) -> Vec<RefreshRecord> {
        let tokens = self
            .tokens
            .lock()
            .expect("gateway refresh-token lock poisoned");
        tokens
            .iter()
            .map(|(token_sha256, entry)| RefreshRecord {
                token_sha256: token_sha256.clone(),
                identity: entry.identity.clone(),
                family: entry.family.clone(),
                inactive_since: entry.inactive_since,
                issued_at: entry.issued_at,
            })
            .collect()
    }

    /// Replace the store's contents with restored records, sweeping anything
    /// that expired while the process was down. Leaves the dirty flag as-is: a
    /// restore is not a mutation worth rewriting the file for.
    pub fn import(&self, records: impl IntoIterator<Item = RefreshRecord>) {
        self.import_at(records, unix_now())
    }

    fn import_at(&self, records: impl IntoIterator<Item = RefreshRecord>, now: u64) {
        let mut tokens = self
            .tokens
            .lock()
            .expect("gateway refresh-token lock poisoned");
        tokens.clear();
        for record in records {
            tokens.insert(
                record.token_sha256,
                RefreshEntry {
                    identity: record.identity,
                    family: record.family,
                    inactive_since: record.inactive_since,
                    issued_at: record.issued_at,
                },
            );
        }
        sweep_refresh_tokens(&mut tokens, now);
    }

    /// Atomically claim the dirty flag; the caller that receives `true` owns
    /// the next write.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// Re-mark the store dirty after a failed write so a later mutation
    /// retries it.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Serialize persistence snapshot-and-write cycles: hold the returned
    /// guard from [`Self::export`] until the state file write completes.
    /// [`Self::take_dirty`] only claims the flag — without this gate two
    /// concurrent saves could export in one order and rename their files in
    /// the other, leaving a pre-revocation snapshot on disk that would
    /// resurrect a replay-revoked token at the next boot.
    pub fn persist_gate(&self) -> MutexGuard<'_, ()> {
        self.persist_gate
            .lock()
            .expect("gateway refresh persist gate poisoned")
    }
}

fn revoke_family(tokens: &mut HashMap<String, RefreshEntry>, family: &str, now: u64) {
    for entry in tokens.values_mut().filter(|entry| entry.family == family) {
        entry.inactive_since.get_or_insert(now);
    }
}

fn sweep_refresh_tokens(tokens: &mut HashMap<String, RefreshEntry>, now: u64) {
    tokens.retain(|_, entry| match entry.inactive_since {
        Some(inactive) => now.saturating_sub(inactive) < REFRESH_TOMBSTONE_TTL_SECS,
        None => now.saturating_sub(entry.issued_at) < REFRESH_IDLE_TTL_SECS,
    });

    let mut by_family: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for (token, entry) in tokens.iter() {
        if let Some(since) = entry.inactive_since {
            by_family
                .entry(entry.family.clone())
                .or_default()
                .push((token.clone(), since));
        }
    }
    for tombstones in by_family.values_mut() {
        if tombstones.len() > MAX_TOMBSTONES_PER_FAMILY {
            tombstones.sort_unstable_by_key(|(_, since)| *since);
            let remove_count = tombstones.len() - MAX_TOMBSTONES_PER_FAMILY;
            for (token, _) in tombstones.drain(..remove_count) {
                tokens.remove(&token);
            }
        }
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
    fn refresh_tokens_are_stored_hashed() {
        let store = RefreshTokenStore::new();
        let token = store.issue(identity());
        let tokens = store.tokens.lock().unwrap();
        assert!(!tokens.contains_key(&token), "raw token must not be a key");
        assert!(tokens.contains_key(&token_sha256(&token)));
    }

    #[test]
    fn refresh_tombstones_are_time_and_count_bounded() {
        let store = RefreshTokenStore::new();
        let now = 1_000_000;
        let mut active = store.issue_at(identity(), now);
        for seconds in 1..=MAX_TOMBSTONES_PER_FAMILY + 2 {
            active = store
                .rotate_at(&active, now + seconds as u64)
                .expect("active token rotates")
                .1;
        }
        assert!(store.tokens.lock().unwrap().len() <= MAX_TOMBSTONES_PER_FAMILY + 1);

        let after_ttl = now + REFRESH_TOMBSTONE_TTL_SECS + 100;
        assert!(
            store.rotate_at(&active, after_ttl).is_none(),
            "an active token idle past the 30-day horizon expires"
        );
        let fresh = store.issue_at(identity(), after_ttl);
        let tokens = store.tokens.lock().unwrap();
        assert_eq!(
            tokens.len(),
            1,
            "expired tombstones and idle active tokens are swept"
        );
        assert!(tokens.contains_key(&token_sha256(&fresh)));
    }

    #[test]
    fn active_token_rotates_just_before_idle_expiry() {
        let store = RefreshTokenStore::new();
        let now = 1_000_000;
        let active = store.issue_at(identity(), now);
        assert!(store
            .rotate_at(&active, now + REFRESH_IDLE_TTL_SECS - 1)
            .is_some());
    }

    #[test]
    fn export_import_round_trips_and_preserves_replay_detection() {
        let store = RefreshTokenStore::new();
        let now = 1_000_000;
        let first = store.issue_at(identity(), now);
        let (_, second) = store.rotate_at(&first, now + 1).expect("rotation");

        let restored = RefreshTokenStore::new();
        restored.import_at(store.export(), now + 2);

        let (restored_identity, third) = restored
            .rotate_at(&second, now + 2)
            .expect("the active token still rotates after a restart");
        assert_eq!(restored_identity, identity());
        assert!(
            restored.rotate_at(&first, now + 3).is_none(),
            "replaying a pre-restart token is rejected"
        );
        assert!(
            restored.rotate_at(&third, now + 4).is_none(),
            "replay detection revokes the restored family"
        );
    }

    #[test]
    fn import_sweeps_records_that_expired_while_down() {
        let store = RefreshTokenStore::new();
        let now = 1_000_000_000;
        store.import_at(
            [
                RefreshRecord {
                    token_sha256: "stale-active".into(),
                    identity: identity(),
                    family: "family-a".into(),
                    inactive_since: None,
                    issued_at: now - REFRESH_IDLE_TTL_SECS,
                },
                RefreshRecord {
                    token_sha256: "stale-tombstone".into(),
                    identity: identity(),
                    family: "family-a".into(),
                    inactive_since: Some(now - REFRESH_TOMBSTONE_TTL_SECS),
                    issued_at: now - REFRESH_TOMBSTONE_TTL_SECS - 1,
                },
                RefreshRecord {
                    token_sha256: "live-active".into(),
                    identity: identity(),
                    family: "family-b".into(),
                    inactive_since: None,
                    issued_at: now - 1,
                },
            ],
            now,
        );
        let tokens = store.tokens.lock().unwrap();
        assert_eq!(tokens.len(), 1);
        assert!(tokens.contains_key("live-active"));
    }

    #[test]
    fn dirty_flag_tracks_mutations_only() {
        let store = RefreshTokenStore::new();
        assert!(!store.take_dirty());
        let token = store.issue(identity());
        assert!(store.take_dirty());
        assert!(!store.take_dirty(), "take_dirty claims the flag");
        assert!(store.rotate(&token).is_some());
        assert!(store.take_dirty());
        assert!(store.rotate("unknown").is_none());
        assert!(!store.take_dirty(), "a failed lookup is not a mutation");
    }
}
