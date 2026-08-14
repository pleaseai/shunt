//! Model/effort capability discovery for the Antigravity CLI.
//!
//! `agy` rejects an invalid model+effort pair outright (`gemini-3.1-pro`
//! accepts only `low` and `high` — passing `medium` fails the run), and the
//! valid set differs per model and changes as Google ships new ones. Rather
//! than hardcode a table that silently rots, discover it from `agy models`,
//! which lists one row per usable combination — a `<model>-<effort>` id, a tab,
//! and a display label (shown here with the tabs expanded):
//!
//! ```text
//! gemini-3.6-flash-high     Gemini 3.6 Flash (High)
//! gemini-3.6-flash-medium   Gemini 3.6 Flash (Medium)
//! gemini-3.1-pro-high       Gemini 3.1 Pro (High)
//! gemini-3.1-pro-low        Gemini 3.1 Pro (Low)
//! claude-sonnet-4-6         Claude Sonnet 4.6 (Thinking)
//! ```
//!
//! Only the first field is parsed; see [`parse_models`]. An id with no effort
//! suffix denotes a model that takes no `--effort` flag. The
//! `Fetching available models...` banner the CLI prints goes to stderr, so it
//! never reaches the parser.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, OnceLock, PoisonError,
    },
    time::{Duration, Instant},
};

/// Cap on the one-off `agy models` discovery call. Generous because the CLI
/// is markedly slower as a gateway subprocess (~20s) than from a shell (~2s),
/// and this runs once per process on the first Antigravity turn.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Effort levels ordered weakest to strongest.
const LADDER: [&str; 3] = ["low", "medium", "high"];

/// Effort used when a route does not pin one. Clamped per model, so a model
/// without `medium` resolves to its nearest supported level.
const DEFAULT_EFFORT: &str = "medium";

pub type EffortMatrix = BTreeMap<String, BTreeSet<String>>;

/// Last successful discovery. Only successes are stored: caching a failure
/// would disable effort validation for the lifetime of the process because one
/// `agy models` call happened to time out.
///
/// A synchronous mutex, deliberately. The lock is only ever taken to clone the
/// map or store a finished one, never across an `.await` — holding it across
/// [`discover`] would serialize every concurrent request behind a ~20s
/// subprocess. Using `std::sync::Mutex` makes that structural rather than a
/// rule to remember, and costs no async machinery on the cache-hit path.
///
/// Poisoning is recovered from rather than treated as a miss: the guarded value
/// is a plain map that a panic cannot leave inconsistent, and swallowing a
/// poisoned lock would disable effort validation for the rest of the process —
/// exactly the failure mode the success-only caching above exists to avoid.
static MATRIX: Mutex<Option<EffortMatrix>> = Mutex::new(None);

/// Set while a discovery subprocess is in flight, so a burst of early requests
/// spawns one `agy models` between them rather than one apiece.
static DISCOVERING: AtomicBool = AtomicBool::new(false);

/// Floor on the gap between discoveries triggered by an unrecognized model.
///
/// [`DISCOVERING`] collapses a concurrent burst but not a steady stream, so
/// without this a route naming an id `agy` does not have would spawn one
/// `agy models` per turn, forever. A model that appears upstream mid-run is
/// not urgent to the minute, and discovery costs ~1.3-1.9s as a gateway
/// subprocess (measured, pleaseai/shunt#337), so a minute is cheap either way.
const UNKNOWN_MODEL_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Monotonic origin, so refresh attempts can be stamped as a plain integer.
/// `Instant::now()` is not const, so the first caller seeds it.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since [`EPOCH`] of the last discovery triggered by an
/// unrecognized model. Zero means never, which is why stamps are floored to 1.
static LAST_UNKNOWN_MODEL_REFRESH: AtomicU64 = AtomicU64::new(0);

/// Model/effort capabilities for the turn's `model`, discovered from
/// `agy models` and cached.
///
/// Never blocks on discovery. A request arriving before the cache is warm gets
/// an empty matrix — which [`resolve_effort`] treats as "not authoritative", so
/// configured values pass through and `agy` validates them itself — and kicks
/// off a background refresh for the turns that follow.
///
/// A populated cache is *not* treated as a closed catalogue. `agy` gains models
/// while the gateway runs, and a cache with no invalidation would make every
/// one of them permanently unknown to a long-running process — turning the
/// documented first-turn cost into an outage lasting until restart
/// (pleaseai/shunt#366). So a miss against a warm cache also schedules a
/// refresh, rate-limited by [`UNKNOWN_MODEL_REFRESH_INTERVAL`] so an id that
/// genuinely does not exist cannot spawn a subprocess per turn.
///
/// This turn still sees the stale matrix: the refresh is deliberately off the
/// request path, matching the rest of this module. The guarantee is that the
/// model becomes unknown *once* rather than forever.
pub async fn effort_matrix(agy_bin: &Path, model: &str) -> EffortMatrix {
    let Some(matrix) = cached() else {
        spawn_refresh(agy_bin);
        return EffortMatrix::new();
    };
    if !matrix.contains_key(model) && claim_unknown_model_refresh() {
        spawn_refresh(agy_bin);
    }
    matrix
}

/// Take the rate-limit slot for an unknown-model refresh, if it is free.
///
/// The compare-and-exchange is what makes a burst of turns naming the same
/// missing model claim once between them rather than once apiece; losing the
/// race means another turn just scheduled the same discovery.
fn claim_unknown_model_refresh() -> bool {
    let now = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64;
    let last = LAST_UNKNOWN_MODEL_REFRESH.load(Ordering::Acquire);
    if !refresh_is_due(last, now, UNKNOWN_MODEL_REFRESH_INTERVAL) {
        return false;
    }
    LAST_UNKNOWN_MODEL_REFRESH
        .compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Whether enough time has passed since `last` to allow another refresh.
///
/// Split out from the atomics so the policy is testable without a process-wide
/// clock: `last == 0` is the never-refreshed sentinel, and the comparison
/// saturates so a clock that appears to move backwards denies rather than
/// panics or wraps into a permanent allow.
fn refresh_is_due(last: u64, now: u64, interval: Duration) -> bool {
    last == 0 || now.saturating_sub(last) >= interval.as_millis() as u64
}

/// Read the cache, holding the lock only for the clone.
fn cached() -> Option<EffortMatrix> {
    MATRIX
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Start a background discovery unless one is already running.
fn spawn_refresh(agy_bin: &Path) {
    if DISCOVERING.swap(true, Ordering::AcqRel) {
        return;
    }
    let agy_bin = agy_bin.to_path_buf();
    tokio::spawn(async move { refresh(&agy_bin).await });
}

/// Discover, then take the lock only long enough to publish a success.
///
/// Expects `DISCOVERING` to have been claimed by the caller; clears it so a
/// failed attempt is retried by the next turn rather than latched forever.
async fn refresh(agy_bin: &Path) {
    let discovered = discover(agy_bin).await;
    if let Some(matrix) = discovered {
        *MATRIX.lock().unwrap_or_else(PoisonError::into_inner) = Some(matrix);
    }
    DISCOVERING.store(false, Ordering::Release);
}

/// Run `agy models` once, returning `None` when it cannot be trusted.
async fn discover(agy_bin: &Path) -> Option<EffortMatrix> {
    let mut command = tokio::process::Command::new(agy_bin);
    command.arg("models");
    // `agy` blocks indefinitely if it inherits a live stdin, which as a
    // gateway subprocess would wedge the request that triggered discovery —
    // no response, no headers, no keepalive.
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Belt and braces: discovery must never be able to stall a turn.
    command.kill_on_drop(true);

    let started = std::time::Instant::now();
    let output = tokio::time::timeout(DISCOVERY_TIMEOUT, command.output()).await;
    let elapsed_ms = started.elapsed().as_millis();
    match output {
        Ok(Ok(out)) if out.status.success() => {
            let matrix = parse_models(&String::from_utf8_lossy(&out.stdout));
            tracing::debug!(
                models = matrix.len(),
                elapsed_ms,
                "discovered agy model efforts"
            );
            Some(matrix)
        }
        Ok(Ok(out)) => {
            tracing::warn!(
                status = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "`agy models` failed; effort values pass through unvalidated this turn"
            );
            None
        }
        Ok(Err(err)) => {
            tracing::warn!(%err, "could not run `agy models`; effort passed through");
            None
        }
        Err(_) => {
            tracing::warn!(
                timeout_s = DISCOVERY_TIMEOUT.as_secs(),
                "`agy models` timed out; effort passed through"
            );
            None
        }
    }
}

/// Populate the cache off the request path.
///
/// Discovery costs ~20s as a gateway subprocess, and no Antigravity request
/// should pay it — [`effort_matrix`] never waits, so until this lands, turns
/// pass their effort through for `agy` to validate. Failure is silent: the
/// flag is cleared, and the next turn schedules another attempt.
pub async fn warm(agy_bin: &Path) {
    if cached().is_some() {
        return;
    }
    if DISCOVERING.swap(true, Ordering::AcqRel) {
        return;
    }
    refresh(agy_bin).await;
}

/// Parse `agy models` output into model → supported efforts.
pub fn parse_models(output: &str) -> EffortMatrix {
    let mut matrix: EffortMatrix = BTreeMap::new();
    for line in output.lines() {
        // Only the first field is the model id. `agy models` may pad a row with
        // a human-readable label after the slug (`gemini-3.1-pro-high    Gemini
        // 3.1 Pro (High)`); suffix-stripping the whole row would then never
        // match an effort, so the label would be registered as its own
        // effort-less model and `gemini-3.1-pro` would be missing entirely.
        // That fails open: unsupported efforts stop being rejected and the
        // default stops being clamped. Blank lines yield no field and are
        // skipped.
        let Some(entry) = line.split_whitespace().next() else {
            continue;
        };
        match LADDER.iter().find_map(|effort| {
            entry
                .strip_suffix(effort)
                .and_then(|model| model.strip_suffix('-'))
                .zip(Some(*effort))
        }) {
            Some((model, effort)) => {
                matrix
                    .entry(model.to_string())
                    .or_default()
                    .insert(effort.to_string());
            }
            // No effort suffix: record the model with an empty effort set so
            // callers can distinguish "takes no effort flag" from "unknown".
            None => {
                matrix.entry(entry.to_string()).or_default();
            }
        }
    }
    matrix
}

/// Outcome of resolving a route's `--effort` against a model's capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortChoice {
    /// Pass this value as `--effort`.
    Use(String),
    /// Send no `--effort` flag and let `agy` speak for itself.
    Omit,
    /// The operator explicitly configured a value this model does not offer.
    Unsupported {
        model: String,
        requested: String,
        supported: Vec<String>,
    },
}

/// Decide the `--effort` argument for a route.
///
/// The distinction that matters is *provenance*, not the value:
///
/// - An **explicitly configured** effort is operator intent. If the model does
///   not offer it, say so ([`EffortChoice::Unsupported`]) rather than quietly
///   substituting a neighbour — silently running `high` for a configured
///   `medium` changes cost, latency and quota while leaving the config file
///   claiming otherwise.
/// - An effort **shunt itself defaulted to** is not operator intent, so
///   clamping it to the nearest supported level is the gateway doing its job.
///   Erroring on our own default would be absurd. Ties break toward the
///   stronger level, preserving the "reasoning tier" reading of the request.
///
/// When the model is unknown — discovery failed, or `agy` gained a model we
/// have not seen — nothing here is authoritative, so defer to the CLI: pass a
/// configured value through, and omit the flag when there is none.
///
/// Deferring is not free, and the cost is worth stating plainly. Measured
/// against the shipped CLI, an id that requires an effort rejects the omitted
/// flag (`--model gemini-3.1-pro requires --effort (available: low, high)`),
/// while one that takes none rejects a supplied default (`--effort is not
/// supported for model "claude-sonnet-4-6"`). A cold matrix cannot tell those
/// two apart, so no choice here is right for both — `agy` at least rejects
/// either before spending tokens. The exposure is the first turn after boot,
/// plus any first turn on a route a reload newly adds, since [`warm`] is gated
/// on the boot config; the next turn, once discovery lands, resolves normally.
pub fn resolve_effort(
    matrix: &EffortMatrix,
    model: &str,
    configured: Option<&str>,
) -> EffortChoice {
    let Some(supported) = matrix.get(model) else {
        return match configured {
            Some(effort) => EffortChoice::Use(effort.to_string()),
            None => EffortChoice::Omit,
        };
    };
    if supported.is_empty() {
        // An empty set is authoritative: discovery positively identified a
        // model that takes no effort flag. This is distinct from the unknown
        // model arm above, where configured values pass through to the CLI.
        return match configured {
            Some(effort) => EffortChoice::Unsupported {
                model: model.to_string(),
                requested: effort.to_string(),
                supported: Vec::new(),
            },
            None => EffortChoice::Omit,
        };
    }
    if let Some(effort) = configured {
        return if supported.contains(effort) {
            EffortChoice::Use(effort.to_string())
        } else {
            EffortChoice::Unsupported {
                model: model.to_string(),
                requested: effort.to_string(),
                supported: supported.iter().cloned().collect(),
            }
        };
    }
    match clamp(supported, DEFAULT_EFFORT) {
        Some(effort) => EffortChoice::Use(effort),
        None => EffortChoice::Omit,
    }
}

/// Nearest supported level to `requested`, preferring the stronger on a tie.
fn clamp(supported: &BTreeSet<String>, requested: &str) -> Option<String> {
    if supported.contains(requested) {
        return Some(requested.to_string());
    }
    let target = LADDER.iter().position(|level| *level == requested)?;
    LADDER
        .iter()
        .enumerate()
        .filter(|(_, level)| supported.contains(**level))
        .min_by_key(|(index, _)| (index.abs_diff(target), usize::from(*index < target)))
        .map(|(_, level)| (*level).to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        claim_unknown_model_refresh, refresh_is_due, resolve_effort, EffortChoice, EffortMatrix,
    };
    use std::time::Duration;

    const INTERVAL: Duration = Duration::from_secs(60);

    #[test]
    fn a_never_refreshed_matrix_is_immediately_due() {
        assert!(refresh_is_due(0, 0, INTERVAL));
    }

    #[test]
    fn a_second_unknown_model_within_the_interval_is_not_due() {
        // The regression that matters: without this, one route naming an id
        // `agy` does not have spawns an `agy models` subprocess per turn.
        assert!(!refresh_is_due(1, 1 + 59_999, INTERVAL));
    }

    #[test]
    fn the_interval_boundary_is_due() {
        assert!(refresh_is_due(1, 1 + 60_000, INTERVAL));
    }

    #[test]
    fn a_backwards_clock_denies_rather_than_wrapping() {
        // saturating_sub yields 0, which is below the interval, so this denies.
        // Wrapping would produce a huge delta and allow every turn through.
        assert!(!refresh_is_due(10_000, 1, INTERVAL));
    }

    #[test]
    fn the_first_claim_wins_and_the_next_is_throttled() {
        // Exercises the atomics, not just the policy. Sole test touching this
        // process-global slot, so it does not race its neighbours.
        assert!(claim_unknown_model_refresh());
        assert!(!claim_unknown_model_refresh());
    }

    #[test]
    fn configured_effort_is_rejected_for_a_model_without_effort_levels() {
        let mut matrix = EffortMatrix::new();
        matrix.insert("fixed-model".to_string(), Default::default());

        assert_eq!(
            resolve_effort(&matrix, "fixed-model", Some("high")),
            EffortChoice::Unsupported {
                model: "fixed-model".to_string(),
                requested: "high".to_string(),
                supported: Vec::new(),
            }
        );
    }

    #[test]
    fn unconfigured_effort_is_omitted_for_a_model_without_effort_levels() {
        let mut matrix = EffortMatrix::new();
        matrix.insert("fixed-model".to_string(), Default::default());

        assert_eq!(
            resolve_effort(&matrix, "fixed-model", None),
            EffortChoice::Omit
        );
    }

    #[test]
    fn configured_effort_passes_through_for_an_unknown_model() {
        assert_eq!(
            resolve_effort(&EffortMatrix::new(), "new-model", Some("ultra")),
            EffortChoice::Use("ultra".to_string())
        );
    }
}
