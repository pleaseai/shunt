//! Bounded session -> account binding table.
//!
//! Pure data structure: it is not synchronized (the owner wraps it in its own
//! mutex) and takes time as a `now` unix-seconds parameter everywhere. Keys are
//! `(provider, SHA-256(selection key))`; the raw selection key is hashed on
//! entry and never stored, logged or exported.

use std::collections::HashMap;
use std::fmt;

use sha2::{Digest, Sha256};

use super::AccountKey;
use crate::config::AuthMode;
use crate::routing::AdapterKind;

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

/// Stable 64-bit digest of an account key: the first 8 bytes (big-endian) of
/// SHA-256 over its serde_json bytes. Computed once per pool health entry and
/// cached there, so a selection does not hash every account again.
pub(crate) fn account_digest(account: &AccountKey) -> u64 {
    let digest = Sha256::digest(account_stable_bytes(account));
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// Rendezvous weight of a session on an account: the splitmix64 finalizer
/// over the session hash's first 8 bytes XOR the account digest. Stable across
/// restarts because no randomized hasher is involved; cheap enough to run for
/// every account on every session selection.
pub(crate) fn weight(session: &SessionHash, account_digest: u64) -> u64 {
    let mut head = [0u8; 8];
    head.copy_from_slice(&session[..8]);
    splitmix64(u64::from_be_bytes(head) ^ account_digest)
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Claude model families whose thinking blocks are bound to the account that
/// produced them, so another account drops them (ADR-0008 Context).
const THINKING_BOUND_MODEL_PREFIXES: [&str; 2] = ["claude-sonnet-5-5", "claude-haiku-5-5"];

/// Whether a request's thinking blocks are lost when its session moves to
/// another account: the `thinking_bound` label of the switch metric (FR-13).
///
/// True only on the Anthropic adapter's Claude subscription pool for the
/// families in [`THINKING_BOUND_MODEL_PREFIXES`]. Kimi shares the Anthropic
/// adapter but signs its own thinking, and the Responses/Codex and xAI pools
/// carry no Anthropic thinking blocks, so every other combination is false.
pub(crate) fn thinking_bound(adapter: &AdapterKind, auth: AuthMode, upstream_model: &str) -> bool {
    *adapter == AdapterKind::Anthropic
        && auth == AuthMode::ClaudeOauth
        && THINKING_BOUND_MODEL_PREFIXES
            .iter()
            .any(|prefix| upstream_model.starts_with(prefix))
}

fn account_stable_bytes(account: &AccountKey) -> Vec<u8> {
    // Plain strings and unit enums only: serialization cannot fail.
    serde_json::to_vec(account).unwrap_or_default()
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
#[cfg_attr(not(test), expect(dead_code, reason = "persisted by T006"))]
pub(crate) struct BindingRow {
    pub(crate) provider: String,
    pub(crate) session_hash: String,
    pub(crate) account: AccountKey,
    pub(crate) last_used: u64,
}

/// Bindings per provider, keyed by session hash. Nested by provider so a
/// lookup borrows the provider name instead of allocating a key.
pub(crate) struct SessionBindings {
    map: HashMap<String, HashMap<SessionHash, Binding>>,
    /// Total bindings across providers.
    len: usize,
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
            .field("len", &self.len)
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
            len: 0,
            max_entries: max_entries.max(1),
            evictions: 0,
            last_sweep: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
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
        let sessions = self.map.get_mut(provider)?;
        let binding = sessions.get(session)?;
        if expired(binding.last_used, now) {
            sessions.remove(session);
            self.len -= 1;
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
        let live = match self
            .map
            .get_mut(provider)
            .and_then(|s| s.get(session).map(|b| (b.last_used, b.generation)))
        {
            Some((last_used, _)) if expired(last_used, now) => {
                self.remove(provider, session);
                None
            }
            other => other.map(|(_, generation)| generation),
        };
        match live {
            None => {
                self.insert_new(provider, *session, served.clone(), now);
                CommitOutcome::Placed
            }
            Some(current) if observed_generation < current => CommitOutcome::Stale,
            Some(_) => {
                let binding = self
                    .map
                    .get_mut(provider)
                    .and_then(|sessions| sessions.get_mut(session))
                    .expect("checked live above");
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

    /// Drop every binding whose account matches; returns how many were removed.
    pub(crate) fn remove_matching(&mut self, matches: impl Fn(&AccountKey) -> bool) -> usize {
        self.retain(|_, b| !matches(&b.account))
    }

    /// Live bindings per account (expired entries not counted).
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the T008 admin view"))]
    pub(crate) fn bound_count(&self, account: &AccountKey, now: u64) -> usize {
        self.bindings()
            .filter(|(_, _, b)| b.account == *account && !expired(b.last_used, now))
            .count()
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "persisted by T006"))]
    pub(crate) fn export(&self, now: u64) -> Vec<BindingRow> {
        self.bindings()
            .filter(|(_, _, b)| !expired(b.last_used, now))
            .map(|(provider, session, b)| BindingRow {
                provider: provider.to_string(),
                session_hash: to_hex(session),
                account: b.account.clone(),
                last_used: b.last_used,
            })
            .collect()
    }

    /// Load persisted rows. Malformed hashes and expired rows are dropped, a
    /// `last_used` in the future is clamped to `now`, and the cap still holds.
    #[cfg_attr(not(test), expect(dead_code, reason = "restored by T006"))]
    pub(crate) fn import(&mut self, rows: impl IntoIterator<Item = BindingRow>, now: u64) {
        for row in rows {
            let Some(session) = from_hex(&row.session_hash) else {
                continue;
            };
            let last_used = row.last_used.min(now);
            if expired(last_used, now) {
                continue;
            }
            match self
                .map
                .get_mut(&row.provider)
                .and_then(|sessions| sessions.get_mut(&session))
            {
                Some(existing) if existing.last_used >= last_used => {}
                Some(existing) => {
                    existing.account = row.account;
                    existing.last_used = last_used;
                }
                None => self.insert_new(&row.provider, session, row.account, last_used),
            }
        }
        self.last_sweep = now;
    }

    fn bindings(&self) -> impl Iterator<Item = (&str, &SessionHash, &Binding)> {
        self.map.iter().flat_map(|(provider, sessions)| {
            sessions
                .iter()
                .map(move |(session, binding)| (provider.as_str(), session, binding))
        })
    }

    /// Keep the bindings `keep` accepts; returns how many were removed.
    fn retain(&mut self, mut keep: impl FnMut(&SessionHash, &Binding) -> bool) -> usize {
        let before = self.len;
        for sessions in self.map.values_mut() {
            sessions.retain(|session, binding| keep(session, binding));
        }
        self.map.retain(|_, sessions| !sessions.is_empty());
        self.len = self.map.values().map(HashMap::len).sum();
        before - self.len
    }

    fn remove(&mut self, provider: &str, session: &SessionHash) {
        if let Some(sessions) = self.map.get_mut(provider) {
            if sessions.remove(session).is_some() {
                self.len -= 1;
            }
        }
    }

    fn insert_new(&mut self, provider: &str, session: SessionHash, account: AccountKey, now: u64) {
        if now.saturating_sub(self.last_sweep) >= SWEEP_INTERVAL_SECS {
            self.retain(|_, b| !expired(b.last_used, now));
            self.last_sweep = now;
        }
        if self.len >= self.max_entries {
            self.evict_lru();
        }
        let binding = Binding {
            account,
            last_used: now,
            generation: 1,
        };
        let previous = match self.map.get_mut(provider) {
            Some(sessions) => sessions.insert(session, binding),
            None => self
                .map
                .entry(provider.to_string())
                .or_default()
                .insert(session, binding),
        };
        if previous.is_none() {
            self.len += 1;
        }
    }

    /// Evict least-recently-used entries until room for one more remains under
    /// ~90 % of the cap, in a single O(n) pass.
    fn evict_lru(&mut self) {
        let target =
            (self.max_entries * EVICT_TARGET_PERCENT / 100).min(self.max_entries.saturating_sub(1));
        let excess = self.len.saturating_sub(target);
        if excess == 0 {
            return;
        }
        let mut stamps: Vec<u64> = self.bindings().map(|(_, _, b)| b.last_used).collect();
        // The `excess`-th oldest stamp: everything strictly older goes, then
        // just enough entries at that stamp to reach `excess`.
        let (_, &mut cutoff, _) = stamps.select_nth_unstable(excess - 1);
        let older = stamps.iter().filter(|&&t| t < cutoff).count();
        let mut at_cutoff = excess - older;
        self.retain(|_, b| {
            if b.last_used < cutoff {
                return false;
            }
            if b.last_used == cutoff && at_cutoff > 0 {
                at_cutoff -= 1;
                return false;
            }
            true
        });
        self.evictions += excess as u64;
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "persisted by T006"))]
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg_attr(not(test), expect(dead_code, reason = "restored by T006"))]
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
