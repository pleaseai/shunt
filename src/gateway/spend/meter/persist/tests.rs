use serde_json::{json, Value};

use super::*;
use crate::gateway::spend::{meter::Check, store::Scope, store::SpendLimit};

/// 2026-10-07 (a Wednesday) 12:34:56 UTC.
const WED: u64 = 1_791_376_496;

fn temp_path(label: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "shunt-spend-counters-{}-{}-{label}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join("gateway-spend.counters.json")
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn cap(user: &str, period: Period, cents: &str) -> SpendLimit {
    SpendLimit {
        id: "spl_t".into(),
        amount: Some(cents.into()),
        created_at: String::new(),
        currency: "USD".into(),
        period,
        scope: Scope::User {
            user_id: user.into(),
        },
        object_type: "spend_limit".into(),
        updated_at: String::new(),
    }
}

fn restored(path: &Path, now: u64) -> SpendMeter {
    let meter = SpendMeter::default();
    meter.restore_from(path, now).unwrap();
    meter
}

#[test]
fn counters_path_is_a_sibling_of_the_caps_file() {
    assert_eq!(
        counters_path(Path::new("/h/.shunt/gateway-spend.json")),
        Path::new("/h/.shunt/gateway-spend.counters.json")
    );
    assert_eq!(
        counters_path(Path::new("/h/caps")),
        Path::new("/h/caps.counters.json")
    );
}

#[test]
fn round_trip_restores_every_window_and_enforcement_uses_it() {
    let path = temp_path("roundtrip");
    let meter = SpendMeter::default();
    meter.record("alice", WED, 700);
    meter.record("alice", WED + 5, 300);
    meter.record("bob", WED, 9);
    assert!(meter.flush_to(&path, 13, WED).unwrap());

    let after = restored(&path, WED);

    for period in PERIODS {
        assert_eq!(after.spent("alice", period, WED), 1000);
        assert_eq!(after.spent("bob", period, WED), 9);
    }
    assert!(matches!(
        after.check(&[cap("alice", Period::Daily, "0")], "alice", WED),
        Check::Blocked { .. }
    ));
    // Restored state is already on disk; nothing to rewrite.
    assert!(!after.flush_to(&path, 13, WED).unwrap());
    let file = read(&path);
    assert_eq!(file["version"], 1);
    assert_eq!(file["counters"].as_array().unwrap().len(), 6);
}

#[test]
fn an_unchanged_meter_writes_nothing() {
    let path = temp_path("clean");
    let meter = SpendMeter::default();
    assert!(!meter.flush_to(&path, 13, WED).unwrap());
    assert!(!path.exists());
    meter.record("alice", WED, 1);
    assert!(meter.flush_to(&path, 13, WED).unwrap());
    assert!(!meter.flush_to(&path, 13, WED).unwrap());
}

#[test]
fn flush_prunes_windows_older_than_the_retention_horizon() {
    let path = temp_path("prune");
    let meter = SpendMeter::default();
    // ~20 months before WED, ~2 months before, and now.
    let old = WED - 600 * DAY;
    let recent = WED - 60 * DAY;
    meter.record("alice", old, 1);
    meter.record("alice", recent, 2);
    meter.record("alice", WED, 4);

    meter.flush_to(&path, 13, WED).unwrap();

    let starts: Vec<u64> = read(&path)["counters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["start"].as_u64().unwrap())
        .collect();
    assert!(starts
        .iter()
        .all(|start| *start >= months_back_start(WED, 13)));
    assert!(!starts.contains(&window(Period::Daily, old).start));
    assert!(starts.contains(&window(Period::Daily, recent).start));
    assert!(starts.contains(&window(Period::Monthly, WED).start));
    // Pruned from memory too.
    assert_eq!(meter.spent("alice", Period::Monthly, old), 0);
}

#[test]
fn pruning_never_drops_a_window_that_is_still_open() {
    let path = temp_path("open");
    // Zero months puts the horizon at this month's start, and the week that
    // straddles the month edge began before it but has not ended.
    let first = window(Period::Monthly, WED).start;
    let monday = window(Period::Weekly, first).start;
    assert!(monday < first, "fixture: week straddles the month start");
    let meter_edge = SpendMeter::default();
    meter_edge.record("alice", first + 3600, 5);
    meter_edge.flush_to(&path, 0, first + 3600).unwrap();
    assert_eq!(
        restored(&path, first + 3600).spent("alice", Period::Weekly, first + 3600),
        5
    );
}

#[test]
fn a_malformed_record_marks_only_its_principal_and_survives_a_rewrite() {
    let path = temp_path("malformed");
    let start = window(Period::Daily, WED).start;
    let bad = json!({"principal": "mallory", "period": "daily", "start": start, "femto": "lots"});
    write(
        &path,
        &json!({"version": 1, "counters": [
            {"principal": "alice", "period": "daily", "start": start, "femto": 11},
            bad,
            {"principal": "bob", "period": "daily", "start": start, "femto": 22},
        ]}),
    );

    let meter = restored(&path, WED);

    assert_eq!(meter.spent("alice", Period::Daily, WED), 11);
    assert_eq!(meter.spent("bob", Period::Daily, WED), 22);
    assert_eq!(meter.check(&[], "mallory", WED), Check::Unavailable);
    assert_eq!(meter.check(&[], "alice", WED), Check::Allow);
    assert_eq!(meter.check(&[], "bob", WED), Check::Allow);

    meter.record("alice", WED, 1);
    meter.flush_to(&path, 13, WED).unwrap();
    let counters = read(&path)["counters"].as_array().unwrap().clone();
    assert!(
        counters.contains(&bad),
        "malformed record carried through untouched"
    );
    assert!(counters
        .iter()
        .any(|record| record["principal"] == "alice" && record["femto"] == 12));
}

#[test]
fn the_unavailable_flag_lifts_once_the_bad_records_windows_elapse() {
    let path = temp_path("lift");
    let start = window(Period::Daily, WED).start;
    write(
        &path,
        &json!({"version": 1, "counters": [
            {"principal": "mallory", "period": "daily", "start": start, "femto": -1},
        ]}),
    );
    let meter = restored(&path, WED);
    let end = window(Period::Daily, WED).end;

    assert_eq!(meter.check(&[], "mallory", end - 1), Check::Unavailable);
    assert_eq!(meter.check(&[], "mallory", end), Check::Allow);
    // Cleared for good, not just for that probe.
    assert_eq!(meter.check(&[], "mallory", end - 1), Check::Allow);
}

#[test]
fn an_unknowable_window_poisons_through_the_longest_current_window() {
    let path = temp_path("unknowable");
    write(
        &path,
        &json!({"version": 1, "counters": [
            {"principal": "mallory", "period": "fortnightly", "femto": 1},
        ]}),
    );
    let meter = restored(&path, WED);
    let latest = PERIODS
        .iter()
        .map(|period| window(*period, WED).end)
        .max()
        .unwrap();

    assert_eq!(meter.check(&[], "mallory", latest - 1), Check::Unavailable);
    assert_eq!(meter.check(&[], "mallory", latest), Check::Allow);
}

#[test]
fn a_stale_malformed_record_flags_nobody_but_is_still_carried() {
    let path = temp_path("stale");
    let old =
        json!({"principal": "mallory", "period": "daily", "start": WED - 3 * DAY, "femto": "x"});
    write(&path, &json!({"version": 1, "counters": [old]}));
    let meter = restored(&path, WED);
    assert_eq!(meter.check(&[], "mallory", WED), Check::Allow);
    meter.record("alice", WED, 1);
    meter.flush_to(&path, 13, WED).unwrap();
    assert!(read(&path)["counters"].as_array().unwrap().contains(&old));
}

#[test]
fn a_record_with_unknown_fields_is_preserved_and_distrusted() {
    let path = temp_path("newer");
    let start = window(Period::Daily, WED).start;
    let newer = json!({"principal": "carol", "period": "daily", "start": start,
                       "femto": 5, "currency_micro": 7});
    write(&path, &json!({"version": 1, "counters": [newer]}));
    let meter = restored(&path, WED);
    assert_eq!(meter.check(&[], "carol", WED), Check::Unavailable);
    meter.record("alice", WED, 1);
    meter.flush_to(&path, 13, WED).unwrap();
    assert!(read(&path)["counters"].as_array().unwrap().contains(&newer));
}

#[test]
fn a_misaligned_or_future_window_is_treated_as_malformed() {
    let path = temp_path("insane");
    let start = window(Period::Daily, WED).start;
    write(
        &path,
        &json!({"version": 1, "counters": [
            {"principal": "dave", "period": "daily", "start": start + 1, "femto": 1},
            {"principal": "erin", "period": "monthly", "start": u64::MAX, "femto": 1},
        ]}),
    );
    let meter = restored(&path, WED);
    assert_eq!(meter.spent("dave", Period::Daily, WED), 0);
    assert_eq!(meter.check(&[], "dave", WED), Check::Unavailable);
    assert_eq!(meter.check(&[], "erin", WED), Check::Unavailable);
}

#[test]
fn an_unreadable_envelope_or_wrong_version_is_a_hard_error() {
    let path = temp_path("envelope");
    std::fs::write(&path, b"not json").unwrap();
    let error = SpendMeter::default().restore_from(&path, WED).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    write(&path, &json!({"version": 2, "counters": []}));
    let error = SpendMeter::default().restore_from(&path, WED).unwrap_err();
    assert!(error.to_string().contains("version mismatch"), "{error}");

    write(&path, &json!({"version": 1}));
    assert!(SpendMeter::default().restore_from(&path, WED).is_err());
}

#[test]
fn an_absent_file_is_a_cold_start() {
    let path = temp_path("absent");
    let meter = restored(&path, WED);
    assert_eq!(meter.spent("alice", Period::Daily, WED), 0);
}

#[test]
fn months_back_start_lands_on_a_month_boundary() {
    // 2026-10-07 minus 13 months -> 2025-09-01 00:00 UTC.
    assert_eq!(months_back_start(WED, 13), 1_756_684_800);
    assert_eq!(
        months_back_start(WED, 0),
        window(Period::Monthly, WED).start
    );
    assert_eq!(months_back_start(WED, u64::MAX), 0);
}
