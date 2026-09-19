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
/// `draw / 2^64` lands in `[0, 1)`, so the running total is never reached
/// exactly and the trailing `unwrap_or` is a floating-point guard rather than a
/// reachable arm. Zero-weight targets are skipped, not shifted: the weights
/// list stays aligned with `targets` whatever an operator disables.
fn arm(router: &RandomRouterConfig, draw: u64) -> &str {
    let total: f64 = router
        .enabled()
        .map(|(index, _)| router.weight(index))
        .sum();
    if total <= 0.0 {
        return first_enabled(router);
    }
    let mut point = (draw as f64 / 2f64.powi(64)) * total;
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
    weights
        .as_ref()
        .map(|weights| weights.iter().map(|weight| weight.to_bits()).collect())
        .unwrap_or_else(Vec::<u64>::new)
        .hash(&mut hasher);
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
        let reseed = streams
            .get(model)
            .is_none_or(|stream| stream.fingerprint != fingerprint);
        if reseed {
            streams.insert(
                model.to_string(),
                Stream {
                    fingerprint,
                    rng: match seed {
                        Some(seed) => rand::rngs::StdRng::seed_from_u64(seed),
                        None => rand::rngs::StdRng::from_os_rng(),
                    },
                },
            );
        }
        streams
            .get_mut(model)
            .expect("the stream was just inserted or already matched")
            .rng
            .random()
    }
}

#[cfg(test)]
mod tests;
