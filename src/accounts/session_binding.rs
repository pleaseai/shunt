//! Bounded session -> account binding table.
//!
//! Pure data structure: it is not synchronized (the owner wraps it in its own
//! mutex) and takes time as a `now` unix-seconds parameter everywhere. Keys are
//! `(provider, SHA-256(selection key))`; the raw selection key is hashed on
//! entry and never stored, logged or exported.

// No non-test callers until the pool selection wiring lands.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;

use sha2::{Digest, Sha256};

use super::AccountKey;

/// A binding idle for longer than this counts as absent.
pub(crate) const BINDING_IDLE_TTL_SECS: u64 = 24 * 60 * 60;
/// Hard cap on bindings across all providers.
pub(crate) const MAX_BINDINGS: usize = 10_000;
/// An over-cap insert evicts down to this share of the cap, so the O(n) pass
/// is amortized over the inserts it makes room for.
const EVICT_TARGET_PERCENT: usize = 90;
/// Minimum spacing between expired-entry sweeps.
const SWEEP_INTERVAL_SECS: u64 = 300;

pub(crate) type SessionHash = [u8; 32];

/// SHA-256 of a selection key. Total over any input, including empty.
pub(crate) fn hash_session_key(key: &str) -> SessionHash {
    Sha256::digest(key.as_bytes()).into()
}

/// Rendezvous weight: first 8 bytes (big-endian) of
/// SHA-256(session hash || serde_json bytes of the account key). Stable across
/// restarts because no randomized hasher is involved.
pub(crate) fn weight(session: &SessionHash, account: &AccountKey) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(session);
    hasher.update(account_stable_bytes(account));
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

fn account_stable_bytes(account: &AccountKey) -> Vec<u8> {
    // Plain strings and unit enums only: serialization cannot fail.
    serde_json::to_vec(account).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BindingKey {
    provider: String,
    session: SessionHash,
}

#[derive(Clone)]
struct Binding {
    account: AccountKey,
    last_used: u64,
    generation: u64,
}

/// What a lookup observed: the bound account and its generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingView {
    pub(crate) account: AccountKey,
    pub(crate) generation: u64,
}

/// Result of [`SessionBindings::commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    /// No binding existed; the session is now bound to the serving account.
    Placed,
    /// The serving account was already bound; `last_used` was refreshed.
    Touched,
    /// The binding moved off `from`; its generation was bumped.
    Moved { from: AccountKey },
    /// The observed generation is older than the binding's: nothing changed.
    Stale,
}

/// One persisted row. `session_hash` is lowercase hex of the SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingRow {
    pub(crate) provider: String,
    pub(crate) session_hash: String,
    pub(crate) account: AccountKey,
    pub(crate) last_used: u64,
}

pub(crate) struct SessionBindings {
    map: HashMap<BindingKey, Binding>,
    max_entries: usize,
    evictions: u64,
    last_sweep: u64,
}

impl Default for SessionBindings {
    fn default() -> Self {
        Self::with_capacity(MAX_BINDINGS)
    }
}

// Hand-written so a Debug print can only ever show counts, not hashes.
impl fmt::Debug for SessionBindings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionBindings")
            .field("len", &self.map.len())
            .field("evictions", &self.evictions)
            .finish()
    }
}

fn expired(last_used: u64, now: u64) -> bool {
    now.saturating_sub(last_used) > BINDING_IDLE_TTL_SECS
}

impl SessionBindings {
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        Self {
            map: HashMap::new(),
            max_entries: max_entries.max(1),
            evictions: 0,
            last_sweep: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn evictions(&self) -> u64 {
        self.evictions
    }

    /// The binding for `(provider, session)`. An entry idle past the TTL is
    /// removed and reported as absent.
    pub(crate) fn lookup(
        &mut self,
        provider: &str,
        session: &SessionHash,
        now: u64,
    ) -> Option<BindingView> {
        let key = BindingKey {
            provider: provider.to_string(),
            session: *session,
        };
        let binding = self.map.get(&key)?;
        if expired(binding.last_used, now) {
            self.map.remove(&key);
            return None;
        }
        Some(BindingView {
            account: binding.account.clone(),
            generation: binding.generation,
        })
    }

    /// Record that `served` answered a request whose selection observed
    /// `observed_generation` (0 when the session was unbound). A commit older
    /// than the current binding changes nothing (NFR-1).
    pub(crate) fn commit(
        &mut self,
        provider: &str,
        session: &SessionHash,
        served: &AccountKey,
        observed_generation: u64,
        now: u64,
    ) -> CommitOutcome {
        let key = BindingKey {
            provider: provider.to_string(),
            session: *session,
        };
        let live = match self.map.get(&key) {
            Some(b) if expired(b.last_used, now) => {
                self.map.remove(&key);
                None
            }
            other => other.map(|b| b.generation),
        };
        match live {
            None => {
                self.insert_new(key, served.clone(), now);
                CommitOutcome::Placed
            }
            Some(current) if observed_generation < current => CommitOutcome::Stale,
            Some(_) => {
                let binding = self.map.get_mut(&key).expect("checked live above");
                binding.last_used = now;
                if binding.account == *served {
                    return CommitOutcome::Touched;
                }
                let from = std::mem::replace(&mut binding.account, served.clone());
                binding.generation += 1;
                CommitOutcome::Moved { from }
            }
        }
    }

    /// Drop every binding to `account`; returns how many were removed.
    pub(crate) fn remove_account(&mut self, account: &AccountKey) -> usize {
        let before = self.map.len();
        self.map.retain(|_, b| b.account != *account);
        before - self.map.len()
    }

    /// Live bindings per account (expired entries not counted).
    pub(crate) fn bound_count(&self, account: &AccountKey, now: u64) -> usize {
        self.map
            .values()
            .filter(|b| b.account == *account && !expired(b.last_used, now))
            .count()
    }

    pub(crate) fn export(&self, now: u64) -> Vec<BindingRow> {
        self.map
            .iter()
            .filter(|(_, b)| !expired(b.last_used, now))
            .map(|(k, b)| BindingRow {
                provider: k.provider.clone(),
                session_hash: to_hex(&k.session),
                account: b.account.clone(),
                last_used: b.last_used,
            })
            .collect()
    }

    /// Load persisted rows. Malformed hashes and expired rows are dropped, a
    /// `last_used` in the future is clamped to `now`, and the cap still holds.
    pub(crate) fn import(&mut self, rows: impl IntoIterator<Item = BindingRow>, now: u64) {
        for row in rows {
            let Some(session) = from_hex(&row.session_hash) else {
                continue;
            };
            let last_used = row.last_used.min(now);
            if expired(last_used, now) {
                continue;
            }
            let key = BindingKey {
                provider: row.provider,
                session,
            };
            match self.map.get_mut(&key) {
                Some(existing) if existing.last_used >= last_used => {}
                Some(existing) => {
                    existing.account = row.account;
                    existing.last_used = last_used;
                }
                None => self.insert_new(key, row.account, last_used),
            }
        }
        self.last_sweep = now;
    }

    fn insert_new(&mut self, key: BindingKey, account: AccountKey, now: u64) {
        if now.saturating_sub(self.last_sweep) >= SWEEP_INTERVAL_SECS {
            self.map.retain(|_, b| !expired(b.last_used, now));
            self.last_sweep = now;
        }
        if self.map.len() >= self.max_entries {
            self.evict_lru();
        }
        self.map.insert(
            key,
            Binding {
                account,
                last_used: now,
                generation: 1,
            },
        );
    }

    /// Evict least-recently-used entries until room for one more remains under
    /// ~90 % of the cap, in a single O(n) pass.
    fn evict_lru(&mut self) {
        let target =
            (self.max_entries * EVICT_TARGET_PERCENT / 100).min(self.max_entries.saturating_sub(1));
        let excess = self.map.len().saturating_sub(target);
        if excess == 0 {
            return;
        }
        let mut order: Vec<(u64, BindingKey)> = self
            .map
            .iter()
            .map(|(k, b)| (b.last_used, k.clone()))
            .collect();
        // Entries at index < excess are the `excess` oldest.
        order.select_nth_unstable_by_key(excess - 1, |(t, _)| *t);
        for (_, key) in order.into_iter().take(excess) {
            self.map.remove(&key);
        }
        self.evictions += excess as u64;
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn from_hex(s: &str) -> Option<SessionHash> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests;
