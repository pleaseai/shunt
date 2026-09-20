//! The pin map and the indexes that order it.
//!
//! Split out of `store.rs` so the store file holds the decide/commit protocol
//! and this one holds the data structure: what a key is, what an entry carries,
//! and the invariant that no write may touch the map without touching the
//! indexes that order it.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use super::super::StageTier;

/// Upper bound on tracked parent sessions. Each entry is well under 100 bytes,
/// so the whole store stays in the low hundreds of kilobytes.
pub(crate) const MAX_TRACKED_SESSIONS: usize = 4096;

/// Upper bound on tracked child pins — the turns of `Task` children, hook
/// agents, and workflow sub-agents — evicted only against each other.
///
/// A separate budget, not a share of [`MAX_TRACKED_SESSIONS`]: eviction takes
/// the least recently decided entry, and a parent blocked on its children is
/// exactly the entry that never refreshes. Under one shared cap a wide fan-out
/// would evict the waiting parents and resume them on the picker default with
/// no dwell gate consulted — the same sub-agent-induced flip the agent-scoped
/// key removes, arriving through eviction instead (ADR-0005 §5).
pub(crate) const MAX_TRACKED_CHILD_PINS: usize = 4096;

/// SHA-256 prefix of an id. Sixteen bytes is the width
/// `accounts::stable_session_index` keeps for the same id.
pub(crate) type IdDigest = [u8; 16];

/// `(advertised model id, digest of the session id, digest of the agent id)`.
///
/// Two router-backed models used inside one Claude Code session keep
/// independent tiers. The ids are hashed and never stored.
///
/// The third element is all zeros for the session's own thread and the digest
/// of `x-claude-code-agent-id` for a delegated turn, so a `Task` child — which
/// sends its parent's session id with its own agent id and its own history —
/// reads and writes a pin of its own. Before it was added, the child's failures
/// escalated the parent and its turns advanced the parent's dwell (ADR-0005
/// fact 4).
pub(crate) type SessionKey = (String, IdDigest, IdDigest);

/// The digest a parent key carries in the agent position.
pub(crate) const PARENT_SCOPE: IdDigest = [0; 16];

/// Which budget an entry counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinScope {
    /// The session's own thread — `main`, and anything without an agent id.
    Parent,
    /// A delegated turn, keyed by its own agent id.
    Child,
}

impl PinScope {
    pub(crate) fn of(key: &SessionKey) -> Self {
        if key.2 == PARENT_SCOPE {
            Self::Parent
        } else {
            Self::Child
        }
    }

    pub(crate) fn cap(self) -> usize {
        match self {
            Self::Parent => MAX_TRACKED_SESSIONS,
            Self::Child => MAX_TRACKED_CHILD_PINS,
        }
    }
}

pub(crate) fn session_key(model: &str, session_id: &str, agent_id: Option<&str>) -> SessionKey {
    (
        model.to_string(),
        digest(session_id),
        agent_id.map_or(PARENT_SCOPE, digest),
    )
}

fn digest(id: &str) -> IdDigest {
    let digest = Sha256::digest(id.as_bytes());
    digest[..16].try_into().expect("SHA-256 prefix is 16 bytes")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StageSession {
    /// Which decision this entry came from, in the order `apply` made them.
    ///
    /// The ordering `StageRouterStore::commit` needs, and `last_seen` cannot
    /// supply: two turns of one session routinely share an `Instant` — the work
    /// between them can be finer than the clock's resolution, and a test injects
    /// one `now` for several turns — so a timestamp comparison cannot tell
    /// "already superseded" from "decided in the same tick", and would drop the
    /// second turn's write.
    pub(crate) seq: u64,
    pub(crate) tier: StageTier,
    /// Hash of the router table in force when this tier was chosen. A config
    /// reload that changes the table invalidates this entry alone, so an
    /// unrelated edit elsewhere in the config does not drop every live pin.
    pub(crate) fingerprint: u64,
    /// Turns served at this tier, counting the one that chose it.
    pub(crate) dwell_turns: u32,
    pub(crate) last_seen: Instant,
    /// The `session_ttl_seconds` of the router that wrote this entry.
    ///
    /// Carried per entry because the store is shared across every
    /// router-backed model, and eviction runs over all of them on whichever
    /// request happened to overflow the cap. Expiring with the *caller's* TTL
    /// would let a model configured with a one-second window delete another
    /// model's seconds-old pin out from under a one-hour window — and an
    /// unpinned session is free to de-escalate immediately, which is the exact
    /// flip the dwell gate exists to prevent.
    pub(crate) ttl: Duration,
    /// The compaction latch (ADR-0005 §11).
    ///
    /// `x-claude-code-context-compacted` is sent once, on the first `main` turn
    /// after a compaction, and libsy's compaction override is meant to hold —
    /// upstream reads the compaction summary, which stays in the prefix. So
    /// the turn that carries the header sets this, every later turn of the
    /// session reads it back into `ToolSignals::compacted`, and it clears with
    /// the pin: on TTL expiry, or when a reload invalidates the entry.
    pub(crate) compacted: bool,
    /// Turns left on a `capable_hold_turns` window (ADR-0005 §6).
    ///
    /// Set by the turn that escalated on signal evidence and decremented by
    /// each non-read-only turn that the hold covers; while it is non-zero the
    /// pin stays on the capable tier whatever the estimate says. `0` — shunt's
    /// default for the key — makes every read of this a no-op, which is what
    /// keeps the shipped hysteresis (`min_dwell_turns` plus
    /// `deescalate_threshold`) the only gate on a default deployment.
    pub(crate) capable_hold_remaining: u32,
    /// Judge calls this session has made under this pin (ADR-0005 §3).
    ///
    /// The per-session half of the bound set — `max_judge_calls` — and the one
    /// that cannot live on the request, because the budget is exactly what a
    /// *sequence* of turns spends. It accumulates on the pin rather than on a
    /// side table so it clears with the pin: on TTL expiry and on a reload that
    /// changes the table, a resumed session gets its budget back, which is the
    /// same lifetime the compaction latch and the dwell count already have.
    pub(crate) judge_calls: u32,
}

/// When an entry falls out of its own window, or `None` when the addition
/// overflows and it therefore never does.
///
/// The mirror of `is_live`'s TTL half: `now.saturating_duration_since(last_seen)
/// > ttl` is `now > last_seen + ttl` wherever the sum exists, including the
/// boundary — a zero TTL read at its own `last_seen` is live under both.
fn expires_at(session: &StageSession) -> Option<Instant> {
    session.last_seen.checked_add(session.ttl)
}

/// The session map and the recency indexes that order it.
///
/// They live in one type because no write may touch only one of them: an
/// order slot left behind by a replaced entry would grow without bound and
/// point eviction at a key whose real recency is newer than the slot claims.
#[derive(Debug, Default)]
pub(crate) struct Entries {
    sessions: HashMap<SessionKey, StageSession>,
    /// `seq` → the key whose current entry carries it, for parent entries.
    ///
    /// `seq` comes from a monotonic counter and every committed turn takes a
    /// fresh one, so this is a total order with no ties, and it is exactly the
    /// order turns were decided — which is the recency order eviction wants.
    /// `last_seen` could not index this: two turns of one session routinely
    /// share an `Instant`, which is the same reason [`StageSession::seq`] exists
    /// at all.
    parents: BTreeMap<u64, SessionKey>,
    /// The same index for child entries. Kept apart so that trimming one
    /// scope's budget pops only that scope's oldest entry — a full child
    /// budget must never reach a parent (see [`MAX_TRACKED_CHILD_PINS`]).
    children: BTreeMap<u64, SessionKey>,
    /// `(expires_at, seq)` for every entry whose TTL can actually elapse, so the
    /// expiry sweep is a prefix of *this* order rather than a walk of the map.
    ///
    /// Separate from the recency orders because recency and expiry are
    /// different orders whenever two routers configure different
    /// `session_ttl_seconds`: the oldest entry can be the live one and a newer
    /// entry the expired one. Sweeping the front of a recency order would then
    /// stop at the live entry, leave the expired one, and let the capacity
    /// trim take the live entry instead — which is a pin the whole-map pass
    /// this replaced would have kept.
    ///
    /// Shared across both scopes: expiry is a property of the entry, not of
    /// its budget. Holds `seq`, not the key, so a returning session still
    /// clones nothing: the recency orders already map `seq` back to its key.
    ///
    /// An entry whose `last_seen + ttl` overflows is simply absent here. Nothing
    /// validates `session_ttl_seconds` against an upper bound, so that addition
    /// is fallible on a config that asks for one; an entry that can never expire
    /// belongs in no expiry index, and the capacity trim still reaches it.
    expiry: BTreeSet<(Instant, u64)>,
}

impl Entries {
    pub(crate) fn get(&self, key: &SessionKey) -> Option<&StageSession> {
        self.sessions.get(key)
    }

    /// Set `compacted` on the entry under `key`, if one is there.
    ///
    /// Deliberately narrower than a general `get_mut`: `compacted` is the one
    /// field no index orders, so writing it cannot desync the recency and
    /// expiry orders this type exists to keep in step with the map. Every
    /// indexed field — `seq`, `last_seen`, `ttl` — is left alone.
    pub(crate) fn latch_compacted(&mut self, key: &SessionKey) {
        if let Some(session) = self.sessions.get_mut(key) {
            session.compacted = true;
        }
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &SessionKey) -> bool {
        self.sessions.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }

    /// How many entries count against `scope`'s budget.
    pub(crate) fn len_of(&self, scope: PinScope) -> usize {
        self.order(scope).len()
    }

    fn order(&self, scope: PinScope) -> &BTreeMap<u64, SessionKey> {
        match scope {
            PinScope::Parent => &self.parents,
            PinScope::Child => &self.children,
        }
    }

    fn order_mut(&mut self, scope: PinScope) -> &mut BTreeMap<u64, SessionKey> {
        match scope {
            PinScope::Parent => &mut self.parents,
            PinScope::Child => &mut self.children,
        }
    }

    /// Retire a `seq` from whichever recency order holds it.
    fn remove_seq(&mut self, seq: u64) -> Option<SessionKey> {
        self.parents
            .remove(&seq)
            .or_else(|| self.children.remove(&seq))
    }

    /// Record a pin, replacing whatever was under that key and retiring its
    /// index slot with it.
    ///
    /// The returning-session path overwrites in place rather than re-inserting,
    /// so the common case — a live session taking its next turn — clones no key.
    /// Only a session the store has not seen pays for one, and it pays it once.
    pub(crate) fn insert(&mut self, key: SessionKey, session: StageSession) {
        let scope = PinScope::of(&key);
        match self.sessions.get_mut(&key) {
            Some(replaced) => {
                let stale = replaced.seq;
                if let Some(at) = expires_at(replaced) {
                    self.expiry.remove(&(at, stale));
                }
                *replaced = session;
                self.order_mut(scope).remove(&stale);
            }
            None => {
                self.sessions.insert(key.clone(), session);
            }
        }
        if let Some(at) = expires_at(&session) {
            self.expiry.insert((at, session.seq));
        }
        self.order_mut(scope).insert(session.seq, key);
    }

    /// Drop every entry whose own TTL has elapsed, cheapest-first.
    ///
    /// Each removal is a pair of `BTreeMap`/`BTreeSet` pops, and each entry is
    /// removed at most once, so the sweep costs O(log n) per entry dropped
    /// rather than a pass over the map. When nothing has expired it is a single
    /// comparison against the front of `expiry`.
    pub(crate) fn drain_expired(&mut self, now: Instant) {
        while let Some(&(at, seq)) = self.expiry.first() {
            if at >= now {
                break;
            }
            self.expiry.pop_first();
            let key = self.remove_seq(seq);
            debug_assert!(
                key.is_some(),
                "the expiry index named a seq no recency index holds"
            );
            if let Some(key) = key {
                self.sessions.remove(&key);
            }
        }
    }

    /// Drop the least recently decided entry of `scope`. `None` once that
    /// scope holds nothing.
    pub(crate) fn remove_oldest(&mut self, scope: PinScope) -> Option<StageSession> {
        let (_, key) = self.order_mut(scope).pop_first()?;
        let session = self.sessions.remove(&key);
        debug_assert!(
            session.is_some(),
            "the recency index named a key the session map does not hold"
        );
        let session = session?;
        if let Some(at) = expires_at(&session) {
            self.expiry.remove(&(at, session.seq));
        }
        Some(session)
    }
}
