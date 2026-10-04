//! On-disk persistence for the spend meter's counters.
//!
//! Counters live in a SIBLING of the stage-1 caps file (`gateway-spend.json` ->
//! `gateway-spend.counters.json`) with their own versioned envelope, so a
//! rollback binary that only knows stage 1 still boots and a counter flush can
//! never rewrite the caps file.
//!
//! ```json
//! {"version": 1,
//!  "counters": [{"principal": "alice", "period": "daily",
//!                "start": 1791331200, "femto": 120000000000000}]}
//! ```
//!
//! A record is carried through byte-for-byte when this build cannot type it
//! (malformed, or carrying fields it does not know), exactly like the stage-1
//! limit and audit records. When the record still names its principal, that
//! principal is flagged unavailable until every window the record could cover
//! has elapsed, so enforcement follows `fail_closed_on_error` instead of
//! silently under-counting; every other record keeps loading. A carried object
//! record whose start cannot date it also gets a reserved `"carried_until"`
//! field (its flag-lift and drop deadline) so a restart does not restart the
//! clock.

use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::{months_back_start, window, SpendMeter, PERIODS};
use crate::{
    gateway::spend::{
        persist::{interleave_records, parse_records},
        store::{OpaqueRecord, Period},
    },
    server::AppState,
};

#[cfg(test)]
mod tests;

const STATE_VERSION: u32 = 1;
const DAY: u64 = 86_400;
/// How often the background task checks for changed counters. Pure debounce:
/// an idle interval writes nothing.
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// A window starting further ahead than this cannot be a real clock reading.
const MAX_FUTURE_SKEW: u64 = 366 * DAY;
/// `spend_retention_months` when `[server.spend]` is absent (never written then).
const DEFAULT_RETENTION_MONTHS: u64 = 13;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CounterRecord {
    principal: String,
    period: Period,
    start: u64,
    femto: u64,
}

#[derive(Serialize)]
struct EnvelopeRef {
    version: u32,
    counters: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct EnvelopeWire {
    version: u32,
    counters: Vec<serde_json::Value>,
}

/// Change tracking, unreadable-record carry-through and flush serialization.
#[derive(Default)]
pub struct PersistState {
    /// Bumped by every recorded charge; a flush writes only when it moved.
    generation: AtomicU64,
    /// The generation the last successful write covered.
    flushed: AtomicU64,
    /// Records this build could not type, preserved verbatim on rewrite.
    opaque: std::sync::Mutex<Vec<HeldRecord>>,
    /// Serializes the periodic and the final flush.
    gate: tokio::sync::Mutex<()>,
}

/// Reserved field a carried object record gets when its own start cannot date
/// it: the unix second its principal flag lifts and the record is dropped.
/// Persisting it keeps a restart from computing a fresh deadline from the new
/// `now` and re-flagging the principal.
const CARRIED_UNTIL: &str = "carried_until";

/// A carried-through record plus the deadline after which it is dropped when
/// its own start cannot date it (persisted inside the record as
/// [`CARRIED_UNTIL`]).
struct HeldRecord {
    record: OpaqueRecord,
    expires_at: Option<u64>,
}

impl PersistState {
    pub(super) fn mark_changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// `<state_path stem>.counters.json` next to the caps file.
pub fn counters_path(state_path: &Path) -> PathBuf {
    let stem = state_path
        .file_stem()
        .map_or_else(|| "gateway-spend".into(), ToOwned::to_owned);
    let mut name = stem;
    name.push(".counters.json");
    state_path.with_file_name(name)
}

struct Loaded {
    typed: Vec<CounterRecord>,
    opaque: Vec<OpaqueRecord>,
}

/// Reads the counters file. `Ok(None)` is an absent file; an unreadable
/// envelope or a version other than [`STATE_VERSION`] is an error.
fn load(path: &Path, now_secs: u64) -> io::Result<Option<Loaded>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let wire: EnvelopeWire = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("gateway spend counters are not valid json: {error}"),
        )
    })?;
    if wire.version != STATE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "gateway spend counters version mismatch: found {}, expected {STATE_VERSION}",
                wire.version
            ),
        ));
    }
    let (parsed, mut opaque) =
        parse_records::<CounterRecord>(wire.counters, path, "spend counter")?;
    let mut typed = Vec::with_capacity(parsed.len());
    for record in parsed {
        if is_sane(&record, now_secs) {
            typed.push(record);
        } else {
            opaque.push(OpaqueRecord {
                typed_before: usize::MAX,
                value: serde_json::to_value(&record).map_err(io::Error::other)?,
            });
        }
    }
    for record in &mut opaque {
        record.typed_before = usize::MAX;
    }
    Ok(Some(Loaded { typed, opaque }))
}

/// A typed record is trusted only if its start is the first instant of a real
/// window of its period and is not from the future.
fn is_sane(record: &CounterRecord, now_secs: u64) -> bool {
    // The first Unix-epoch Monday is day 4; an earlier weekly start is no window.
    let before_first_monday = record.period == Period::Weekly && record.start < 4 * DAY;
    record.start <= now_secs.saturating_add(MAX_FUTURE_SKEW)
        && !before_first_monday
        && window(record.period, record.start).start == record.start
}

/// The instant every window the unreadable `value` could cover has elapsed.
/// A readable start bounds it by the longest window containing that start;
/// otherwise the longest windows containing `now_secs`.
fn lift_after(value: &serde_json::Value, now_secs: u64) -> u64 {
    let anchor = usable_start(value, now_secs).unwrap_or(now_secs);
    let periods = value
        .get("period")
        .and_then(|period| serde_json::from_value::<Period>(period.clone()).ok());
    match periods {
        Some(period) => window(period, anchor).end,
        None => PERIODS
            .iter()
            .map(|period| window(*period, anchor).end)
            .max()
            .unwrap_or(anchor),
    }
}

/// The readable, not-from-the-future start of an unreadable record, if any.
fn usable_start(value: &serde_json::Value, now_secs: u64) -> Option<u64> {
    value
        .get("start")
        .and_then(serde_json::Value::as_u64)
        .filter(|start| *start <= now_secs.saturating_add(MAX_FUTURE_SKEW))
}

/// Wraps a freshly loaded opaque record. One with a usable start keeps the
/// retention rule. One without (no start, unparseable, or beyond the skew
/// bound) expires at its persisted [`CARRIED_UNTIL`] when that parses, else at
/// the lift deadline computed now, which is written into the record so the
/// next flush persists it. A record that is not a JSON object cannot carry the
/// field, so it expires at once (dropped at the first flush after load).
/// The flag is whether the record gained a `carried_until` it did not have, so
/// the file is out of date and the next flush must write it.
fn hold(mut record: OpaqueRecord, now_secs: u64) -> (HeldRecord, bool) {
    let carried = record
        .value
        .get(CARRIED_UNTIL)
        .and_then(serde_json::Value::as_u64)
        // A deadline no real clock could reach is hand-edited: treat it as absent.
        .filter(|until| *until <= now_secs.saturating_add(MAX_FUTURE_SKEW));
    // A stamped deadline wins over re-reading the start: a start that was
    // beyond the skew when the record was stamped may be usable by now, and
    // must not turn the deadline into the start's own far-off window end.
    if carried.is_none() && usable_start(&record.value, now_secs).is_some() {
        let held = HeldRecord {
            record,
            expires_at: None,
        };
        return (held, false);
    }
    let deadline = carried.unwrap_or_else(|| lift_after(&record.value, now_secs));
    let expires_at = match record.value.as_object_mut() {
        Some(object) => {
            object.insert(CARRIED_UNTIL.into(), deadline.into());
            deadline
        }
        None => 0,
    };
    let stamped = carried.is_none() && record.value.is_object();
    let held = HeldRecord {
        record,
        expires_at: Some(expires_at),
    };
    (held, stamped)
}

/// True for a counter window past the retention horizon and already over; a
/// window still open is never pruned, whatever the retention says.
fn expired_window(period: Period, start: u64, cutoff: u64, now_secs: u64) -> bool {
    start < cutoff && window(period, start).end <= now_secs
}

/// True for a carried-through record old enough to drop at the next flush. A
/// record held with a deadline (its start could not date it when loaded)
/// always expires at that (persisted) deadline, even if its start has since
/// come inside the skew bound, so it cannot flag its principal forever; any
/// other follows the retention cutoff.
fn opaque_expired(held: &HeldRecord, cutoff: u64, now_secs: u64) -> bool {
    if let Some(deadline) = held.expires_at {
        return now_secs >= deadline;
    }
    usable_start(&held.record.value, now_secs)
        .is_some_and(|start| start < cutoff && start.saturating_add(32 * DAY) <= now_secs)
}

impl SpendMeter {
    /// Folds a loaded file into this meter. Typed records add to the counters;
    /// each opaque record flags its principal (when readable) and is kept for
    /// the next write.
    fn apply_loaded(&self, loaded: Loaded, now_secs: u64) {
        {
            let mut counters = self.counters.lock().expect("spend meter lock poisoned");
            for record in loaded.typed {
                let slot = counters
                    .entry((record.principal, record.period, record.start))
                    .or_insert(0);
                *slot = slot.saturating_add(record.femto);
            }
        }
        let (held, stamped): (Vec<HeldRecord>, Vec<bool>) = loaded
            .opaque
            .into_iter()
            .map(|record| hold(record, now_secs))
            .unzip();
        if stamped.contains(&true) {
            self.persist.mark_changed();
        }
        for held in &held {
            let until = held
                .expires_at
                .unwrap_or_else(|| lift_after(&held.record.value, now_secs));
            match held.record.value.get("principal").and_then(|p| p.as_str()) {
                Some(principal) if until > now_secs => {
                    tracing::warn!(
                        principal,
                        until,
                        "unreadable spend counter record; principal unavailable until its windows elapse"
                    );
                    self.mark_unavailable_until(principal, until);
                }
                Some(_) => {}
                None => tracing::warn!(
                    "unreadable spend counter record names no principal; preserved, no principal flagged"
                ),
            }
        }
        self.persist
            .opaque
            .lock()
            .expect("spend meter lock poisoned")
            .extend(held);
    }

    /// Loads `path` into this meter (see [`load`]). Public for the restart test
    /// harness; the server uses [`restore`].
    pub fn restore_from(&self, path: &Path, now_secs: u64) -> io::Result<()> {
        if let Some(loaded) = load(path, now_secs)? {
            let count = loaded.typed.len() + loaded.opaque.len();
            self.apply_loaded(loaded, now_secs);
            tracing::info!(path = %path.display(), records = count, "restored spend counters from disk");
        }
        Ok(())
    }

    /// The in-memory half of [`flush_to`](Self::flush_to): prunes windows (and
    /// carried records) past the retention horizon without writing a file.
    /// Returns whether anything was removed. This is what a memory-only meter
    /// runs on every tick, so its counters do not grow for the process
    /// lifetime.
    pub fn prune_to(&self, retention_months: u64, now_secs: u64) -> bool {
        let cutoff = months_back_start(now_secs, retention_months);
        let mut counters = self.counters.lock().expect("spend meter lock poisoned");
        let before = counters.len();
        counters.retain(|(_, period, start), _| !expired_window(*period, *start, cutoff, now_secs));
        let mut pruned = counters.len() != before;
        drop(counters);
        let mut opaque = self
            .persist
            .opaque
            .lock()
            .expect("spend meter lock poisoned");
        let before = opaque.len();
        opaque.retain(|held| !opaque_expired(held, cutoff, now_secs));
        pruned |= opaque.len() != before;
        pruned
    }

    /// Prunes windows that ended before the retention horizon on every call,
    /// then writes the counters to `path` if they changed since the last write
    /// or the prune removed anything. Returns whether a file was written. The
    /// counters lock is held only to copy.
    pub fn flush_to(&self, path: &Path, retention_months: u64, now_secs: u64) -> io::Result<bool> {
        let generation = self.persist.generation.load(Ordering::Acquire);
        let cutoff = months_back_start(now_secs, retention_months);
        let mut pruned = false;
        let mut typed: Vec<CounterRecord> = {
            let mut counters = self.counters.lock().expect("spend meter lock poisoned");
            let before = counters.len();
            // A window still open is never pruned, whatever the retention says.
            counters
                .retain(|(_, period, start), _| !expired_window(*period, *start, cutoff, now_secs));
            pruned |= counters.len() != before;
            counters
                .iter()
                .map(|((principal, period, start), femto)| CounterRecord {
                    principal: principal.clone(),
                    period: *period,
                    start: *start,
                    femto: *femto,
                })
                .collect()
        };
        typed.sort_by(|a, b| {
            (&a.principal, a.start, a.period as u8).cmp(&(&b.principal, b.start, b.period as u8))
        });
        let opaque = {
            let mut opaque = self
                .persist
                .opaque
                .lock()
                .expect("spend meter lock poisoned");
            let before = opaque.len();
            opaque.retain(|held| !opaque_expired(held, cutoff, now_secs));
            pruned |= opaque.len() != before;
            opaque
                .iter()
                .map(|held| held.record.clone())
                .collect::<Vec<_>>()
        };
        if !pruned && generation == self.persist.flushed.load(Ordering::Acquire) {
            return Ok(false);
        }
        let envelope = EnvelopeRef {
            version: STATE_VERSION,
            counters: interleave_records(&typed, &opaque)?,
        };
        let json = serde_json::to_vec_pretty(&envelope).map_err(io::Error::other)?;
        if let Err(error) = crate::atomic_file::write_private_atomic(path, &json) {
            // The prune already left memory; make the retry rewrite the file.
            if pruned {
                self.persist.mark_changed();
            }
            return Err(error);
        }
        self.persist.flushed.fetch_max(generation, Ordering::AcqRel);
        Ok(true)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn path_of(state: &AppState) -> Option<PathBuf> {
    state.gateway_stores.spend.state_path().map(counters_path)
}

fn retention_months(state: &AppState) -> u64 {
    // Re-snapshot so a hot-reloaded `spend_retention_months` applies.
    state
        .refreshed()
        .config
        .server
        .spend
        .as_ref()
        .map_or(DEFAULT_RETENTION_MONTHS, |spend| {
            spend.spend_retention_months
        })
}

/// Boot-time restore, alongside the stage-1 caps restore. An unreadable
/// envelope or a version mismatch aborts startup; an unreadable record only
/// flags its own principal. A no-op when the spend state is memory-only.
pub async fn restore(state: &AppState) -> io::Result<()> {
    let Some(path) = path_of(state) else {
        return Ok(());
    };
    let stores = state.gateway_stores.clone();
    tokio::task::spawn_blocking(move || stores.spend.meter().restore_from(&path, now_secs()))
        .await
        .map_err(|error| {
            io::Error::other(format!("spend counter restore task panicked: {error}"))
        })?
}

/// Writes changed counters off the async workers (or, memory-only, just
/// prunes them). Failure is logged and retried on the next tick; it never
/// reaches a request.
pub async fn flush(state: &AppState) {
    let months = retention_months(state);
    let stores = state.gateway_stores.clone();
    let Some(path) = path_of(state) else {
        // Memory-only: prune, write nothing.
        let pruner = stores.clone();
        if let Err(error) =
            tokio::task::spawn_blocking(move || pruner.spend.meter().prune_to(months, now_secs()))
                .await
        {
            tracing::warn!(%error, "spend counter pruner task panicked");
        }
        return;
    };
    let _gate = stores.spend.meter().persist.gate.lock().await;
    let writer = stores.clone();
    let result = tokio::task::spawn_blocking(move || {
        writer.spend.meter().flush_to(&path, months, now_secs())
    })
    .await;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(%error, "failed to persist spend counters"),
        Err(error) => tracing::warn!(%error, "spend counter persister task panicked"),
    }
}

/// Spawns the debounced background flush loop. Memory-only, the same tick
/// prunes and writes no file.
pub fn spawn_flusher(state: AppState) {
    if path_of(&state).is_some() {
        tracing::info!(
            interval_secs = FLUSH_INTERVAL.as_secs(),
            "spend counter persistence enabled"
        );
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            flush(&state).await;
        }
    });
}

/// Last flush after the server stopped accepting, bounded by the
/// caller-supplied `bound` (the server passes a fixed 5 s).
pub async fn flush_final(state: &AppState, bound: Duration) {
    if tokio::time::timeout(bound, flush(state)).await.is_err() {
        tracing::warn!(
            timeout_seconds = bound.as_secs(),
            "final spend counter flush timed out; the last interval of spend may be lost"
        );
    }
}
