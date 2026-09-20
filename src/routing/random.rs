//! `type = "random"` — a weighted split across public model ids (ADR-0005 §2).
//!
//! Two selection modes, one arithmetic. Both draw a `u64`, scale it into the
//! cumulative positive-weight total, and walk the enabled targets; they differ
//! only in where the `u64` comes from:
//!
//! * `affinity = "session"` with a non-blank `x-claude-code-session-id` hashes
//!   `seed ‖ model ‖ session_id`. Nothing is stored, so the arm survives a
//!   restart, is identical on every replica loading the same config, and the
//!   session's `count_tokens` probes land where its turns do.
//! * everything else — `affinity = "request"`, and a session-affinity request
//!   with no session id — takes the next value from a per-router RNG, seeded
//!   from `seed` when the operator set one so the sequence reproduces.
//!
//! A sessionless request must *not* hash the blank id: every such caller would
//! share one key, and a configured 90/10 split would be 100/0 for them.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};

use crate::config::{RandomAffinity, RandomRouterConfig};
use crate::routing::context::RouterContext;
use crate::routing::outcome::RouteSource;
use crate::routing::stage::StageContext;

/// Pick the target that serves this request.
///
/// `context` is `None` for the body-less entry points — `/routes`, discovery,
/// and the public [`crate::routing::resolve_model`] — which have no session to
/// pin and must answer the same thing twice. They get the first enabled target
/// rather than a draw: a discovery listing that reported a different
/// destination on each call would be describing nothing.
pub(crate) fn select<'a>(
    router: &'a RandomRouterConfig,
    model: &str,
    context: Option<&StageContext<'_>>,
) -> (&'a str, RouteSource) {
    let Some(context) = context else {
        return (first_enabled(router), RouteSource::Random);
    };
    if router.affinity == RandomAffinity::Session {
        let hints = RouterContext::from_headers(context.headers);
        if let Some(session_id) = hints.session_id.filter(|id| !id.trim().is_empty()) {
            return (
                arm(router, session_hash(router.seed, model, session_id)),
                RouteSource::RandomSession,
            );
        }
    }
    let draw = context
        .store
        .random_draw(model, fingerprint(router), router.seed);
    (arm(router, draw), RouteSource::Random)
}

/// The first target with a positive weight.
///
/// A zero weight disables a target, so it is not the answer here either — an
/// operator who parked an arm at `0` has said it must not be served, and a
/// body-less surface reporting it would advertise a destination no request can
/// reach.
fn first_enabled(router: &RandomRouterConfig) -> &str {
    router
        .enabled()
        .next()
        .map(|(_, target)| target)
        // Validation rejects an empty `targets` and an all-zero `weights`, so
        // this is unreachable for a loaded config; a `RandomRouterConfig` built
        // directly in code could still hit it, and routing the request to a
        // blank id (which falls through to the default provider) beats a panic
        // on the request path.
        .unwrap_or("")
}

/// `sha256(seed_le_bytes ‖ model ‖ session_id)`, first eight bytes as a `u64`.
///
/// A pure function of config, model id, and session id: no store access, no
/// clock, no process state. `seed` defaults to `0` rather than being omitted so
/// that setting `seed = 0` and leaving the key out are the same configuration
/// — an operator reading the reference page should not find a third behaviour
/// hiding between those two.
///
/// The ids are hashed without a separator. Length-extension between the two is
/// not a concern the way it would be for a MAC: both inputs are the operator's
/// own model id and the client's own session id, and a caller who can pick a
/// session id that lands on the arm it wants could equally have requested that
/// target by name — session affinity is stickiness, not access control
/// (ADR-0005 §2). Tier access is the managed-model policy's job.
fn session_hash(seed: Option<u64>, model: &str, session_id: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.unwrap_or(0).to_le_bytes());
    hasher.update(model.as_bytes());
    hasher.update(session_id.as_bytes());
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// Scale `draw` into the cumulative positive-weight total and walk the enabled
/// targets.
///
/// The draw is scaled through the top 53 bits rather than all 64: `f64` carries
/// a 53-bit mantissa, so `draw as f64` rounds — and `u64::MAX` rounds *up* to
/// exactly `2^64`, which would put `point` at `total` and silently hand that
/// draw to the trailing `last`. `draw >> 11` is exact in an `f64`, so the
/// quotient lands in `[0, 1)` for every draw and the running total is never
/// reached: the trailing `last` is a floating-point guard rather than a
/// reachable arm. Zero-weight targets are skipped, not shifted: the weights
/// list stays aligned with `targets` whatever an operator disables.
fn arm(router: &RandomRouterConfig, draw: u64) -> &str {
    let total: f64 = router
        .enabled()
        .map(|(index, _)| router.weight(index))
        .sum();
    // `is_finite` as well as positive: validation rejects a weight list whose
    // total overflows to infinity, so this cannot fire on a loaded config, but
    // an infinite `total` would make every `point < weight` false and collapse
    // the split onto the last enabled target rather than failing.
    if !(total > 0.0 && total.is_finite()) {
        return first_enabled(router);
    }
    // 2^53, the largest integer an `f64` represents exactly.
    let mut point = ((draw >> 11) as f64 / 9_007_199_254_740_992.0) * total;
    let mut last = first_enabled(router);
    for (index, target) in router.enabled() {
        let weight = router.weight(index);
        if point < weight {
            return target;
        }
        point -= weight;
        last = target;
    }
    last
}

/// Hash the selection inputs a per-router RNG is keyed on.
///
/// Only `targets`, `weights`, and `seed`: those three decide the sequence, so a
/// hot reload that changes any of them must reseed rather than continue a
/// stream drawn against a different distribution. `affinity` is deliberately
/// out — flipping it does not change what a draw means, only whether one is
/// taken.
fn fingerprint(router: &RandomRouterConfig) -> u64 {
    use std::hash::{Hash, Hasher};

    // Destructured rather than dotted, so a key added to the table later fails
    // to compile here instead of silently sharing a stale RNG with the old one.
    let RandomRouterConfig {
        targets,
        weights,
        seed,
        affinity: _,
    } = router;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    targets.hash(&mut hasher);
    // `f64` is not `Hash`; `to_bits` is, and distinguishes the values that
    // matter here. Two NaN spellings would hash apart, which validation has
    // already rejected.
    //
    // Hashed element by element rather than collected: `fingerprint` runs on
    // every request routed through a `random` entry, and a temporary `Vec<u64>`
    // per turn buys nothing. The length prefix keeps the digest identical to
    // the slice's own `Hash` — `<[T]>::hash` writes `len` and then each
    // element — so `None` and an empty list stay distinguishable from a
    // one-element one.
    match weights.as_ref() {
        Some(weights) => {
            weights.len().hash(&mut hasher);
            for weight in weights {
                weight.to_bits().hash(&mut hasher);
            }
        }
        None => 0usize.hash(&mut hasher),
    }
    seed.hash(&mut hasher);
    hasher.finish()
}

/// Per-router RNG state, held on `StageRouterStore` beside the tier pins.
///
/// One entry per `[[models]]` id, replaced when the selection inputs change, so
/// a long-running process with a reloaded config keeps exactly one stream per
/// router rather than accumulating one per generation.
#[derive(Default)]
pub(crate) struct DrawState {
    streams: Mutex<HashMap<String, Stream>>,
}

struct Stream {
    fingerprint: u64,
    rng: rand::rngs::StdRng,
}

impl Stream {
    /// A stream keyed to `fingerprint`. A configured `seed` reproduces the
    /// sequence across restarts; without one it is entropy-backed.
    fn new(fingerprint: u64, seed: Option<u64>) -> Self {
        Self {
            fingerprint,
            rng: match seed {
                Some(seed) => rand::rngs::StdRng::seed_from_u64(seed),
                None => rand::rngs::StdRng::from_os_rng(),
            },
        }
    }
}

impl std::fmt::Debug for DrawState {
    /// Hand-written because `StdRng` is not `Debug` — and would not be worth
    /// printing if it were: the state that matters is which fingerprint each
    /// stream was seeded for.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DrawState").finish_non_exhaustive()
    }
}

impl DrawState {
    /// The next `u64` for one router, seeding the stream on first use and
    /// reseeding it when the selection inputs changed.
    ///
    /// A configured `seed` reproduces the sequence across restarts, which is
    /// what `affinity = "request"` promises; without one the stream is
    /// entropy-backed.
    pub(crate) fn next(&self, model: &str, fingerprint: u64, seed: Option<u64>) -> u64 {
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        // One lookup and no key allocation on the path every request takes.
        // `get_mut` borrows the stream already there, a reload writes the new
        // stream through that same borrow, and only a model seen for the first
        // time in this process pays the `to_string`. The draw itself stays
        // inside the lock because it is a few ns of CPU on a `StdRng` the
        // borrow owns — releasing and re-taking the lock around it would cost
        // more than it saves and would let two turns of one session draw the
        // same value.
        if let Some(stream) = streams.get_mut(model) {
            if stream.fingerprint != fingerprint {
                *stream = Stream::new(fingerprint, seed);
            }
            return stream.rng.random();
        }
        streams
            .entry(model.to_string())
            .or_insert_with(|| Stream::new(fingerprint, seed))
            .rng
            .random()
    }
}

#[cfg(test)]
mod tests;
