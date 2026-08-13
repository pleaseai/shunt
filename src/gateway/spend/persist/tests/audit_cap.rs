use std::fs;

use serde_json::json;

use super::super::save_snapshot;
use crate::gateway::spend::store::{
    AuditRecord, OpaqueRecord, Period, Scope, SpendState, SpendStore, MAX_AUDIT_RECORDS,
};

fn audit_record(id: u64) -> AuditRecord {
    AuditRecord {
        id,
        created_at: format!("2026-08-10T00:00:{id:02}.000Z"),
        actor: "admin-key:test".into(),
        before: None,
        after: None,
    }
}

fn opaque_audit(id: u64, typed_before: usize) -> OpaqueRecord {
    OpaqueRecord {
        typed_before,
        value: json!({"id": id, "future_event": true}),
    }
}

fn append_mutation(state: SpendState) -> SpendState {
    SpendStore::upsert_state(
        state,
        Scope::Organization,
        Period::Monthly,
        Some("100".into()),
        "admin-key:test",
        "2026-08-10T00:01:00.000Z".into(),
    )
    .0
}

fn saved_audit_ids(state: &SpendState, label: &str) -> Vec<u64> {
    let path = super::temp_file(label);
    save_snapshot(&path, state).expect("save state");
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read state")).expect("parse state");
    let ids = saved["audit"]
        .as_array()
        .expect("audit array")
        .iter()
        .map(|record| record["id"].as_u64().expect("audit id"))
        .collect();
    fs::remove_dir_all(path.parent().unwrap()).ok();
    ids
}

#[test]
fn mixed_audit_records_are_capped_by_total_count_in_original_order() {
    let typed_count = MAX_AUDIT_RECORDS - 2;
    let state = SpendState {
        audit: (2..=typed_count as u64 + 1).map(audit_record).collect(),
        opaque_audit: vec![
            opaque_audit(1, 0),
            opaque_audit(MAX_AUDIT_RECORDS as u64, typed_count),
        ],
        next_audit_id: MAX_AUDIT_RECORDS as u64 + 1,
        ..SpendState::default()
    };

    let state = append_mutation(state);
    assert_eq!(
        state.audit.len() + state.opaque_audit.len(),
        MAX_AUDIT_RECORDS
    );
    assert_eq!(state.next_audit_id, MAX_AUDIT_RECORDS as u64 + 2);

    let ids = saved_audit_ids(&state, "mixed-audit-cap");
    assert_eq!(ids, (2..=MAX_AUDIT_RECORDS as u64 + 1).collect::<Vec<_>>());
}

#[test]
fn all_opaque_audit_records_are_capped_when_a_typed_record_is_appended() {
    let state = SpendState {
        opaque_audit: (1..=MAX_AUDIT_RECORDS as u64)
            .map(|id| opaque_audit(id, 0))
            .collect(),
        next_audit_id: MAX_AUDIT_RECORDS as u64 + 1,
        ..SpendState::default()
    };

    let state = append_mutation(state);
    assert_eq!(
        state.audit.len() + state.opaque_audit.len(),
        MAX_AUDIT_RECORDS
    );
    assert_eq!(state.opaque_audit.len(), MAX_AUDIT_RECORDS - 1);

    let ids = saved_audit_ids(&state, "all-opaque-audit-cap");
    assert_eq!(ids.len(), MAX_AUDIT_RECORDS);
    assert_eq!(ids.first(), Some(&2));
    assert_eq!(ids.last(), Some(&(MAX_AUDIT_RECORDS as u64 + 1)));
}
