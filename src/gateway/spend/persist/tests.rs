use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::gateway::spend::store::{Period, Scope, SpendStore};

mod audit_cap;

fn temp_file(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "shunt-spend-persist-{}-{}-{label}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&directory).expect("create temp directory");
    directory.join("state.json")
}

fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
    use std::io::{self, Write};

    struct BufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }
    impl Write for BufferWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let output = Arc::new(Mutex::new(Vec::new()));
    let writer_output = Arc::clone(&output);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || BufferWriter {
            buffer: Arc::clone(&writer_output),
        })
        .with_ansi(false)
        .without_time()
        .finish();
    let result = tracing::subscriber::with_default(subscriber, run);
    let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    (result, logs)
}

/// `[server.admin]` + `[server.spend]` with no `[server.gateway]`: spend
/// persistence is configured from `[server.spend].state_path` alone.
fn app_state_with_path(path: std::path::PathBuf, tokens_env: &str) -> AppState {
    let mut config = crate::config::Config::default();
    config.server.admin = Some(crate::config::AdminConfig {
        header: "x-shunt-admin-token".into(),
        tokens_env: tokens_env.to_string(),
        tokens_file: None,
        write_keys: vec![crate::config::AdminKey {
            id: "writer".into(),
            key: "write-key-0123456789abcdef0123456789".into(),
        }],
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        oidc: None,
    });
    config.server.spend = Some(crate::config::SpendConfig {
        state_path: Some(path),
        ..crate::config::SpendConfig::default()
    });
    crate::server::AppState::new(config, reqwest::Client::new()).expect("build app state")
}

#[test]
fn round_trip_preserves_limits_and_audit_records() {
    let path = temp_file("roundtrip");
    let store = SpendStore::new(None);
    store.upsert(
        Scope::Organization,
        Period::Monthly,
        Some("50000".into()),
        "admin-key:test",
        "2026-08-09T00:00:00.000Z".into(),
    );
    let snapshot = store.export();
    save_snapshot(&path, &snapshot).expect("save state");

    let loaded = load(&path).expect("load state").expect("state exists");
    assert_eq!(loaded.state.limits, snapshot.limits);
    assert_eq!(loaded.state.audit, snapshot.audit);
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn app_state_restore_populates_the_process_lifetime_store() {
    let _env_guard = crate::config::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_file("app-state");
    let source = SpendStore::new(None);
    let expected = source.upsert(
        Scope::User {
            user_id: "usr_test".into(),
        },
        Period::Weekly,
        Some("750".into()),
        "admin-key:test",
        "2026-08-09T00:00:00.000Z".into(),
    );
    save_snapshot(&path, &source.export()).expect("save state");

    let tokens_env = format!("SHUNT_SPEND_RESTORE_TOKENS_{}", std::process::id());
    std::env::remove_var(&tokens_env);
    let state = app_state_with_path(path.clone(), &tokens_env);
    let runtime = tokio::runtime::Runtime::new().expect("build runtime");
    runtime.block_on(restore(&state)).expect("restore state");

    assert_eq!(state.gateway_stores.spend.get(&expected.id), Some(expected));
    assert_eq!(state.gateway_stores.spend.export().audit.len(), 1);

    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn invalid_persisted_limits_are_hidden_individually_and_warned() {
    let cases = [
        ("amount-negative", "amount", json!({"amount":"-5"})),
        ("amount-text", "amount", json!({"amount":"abc"})),
        (
            "amount-long",
            "amount",
            json!({"amount":(super::super::store::MAX_AMOUNT as u128 + 1).to_string()}),
        ),
        ("currency", "currency", json!({"currency":"EUR"})),
        ("object-type", "type", json!({"type":"nonsense"})),
        (
            "user-empty",
            "scope.user_id",
            json!({"scope":{"type":"user","user_id":""}}),
        ),
        (
            "user-long",
            "scope.user_id",
            json!({"scope":{"type":"user","user_id":"u".repeat(super::super::store::MAX_USER_ID_LENGTH + 1)}}),
        ),
    ];

    for (label, field, patch) in cases {
        let path = temp_file(label);
        let valid = serde_json::json!({
            "id":"spl_valid",
            "amount":"100",
            "created_at":"2026-08-10T00:00:00.000Z",
            "currency":"USD",
            "period":"monthly",
            "scope":{"type":"organization"},
            "type":"spend_limit",
            "updated_at":"2026-08-10T00:00:00.000Z"
        });
        let mut invalid = valid.clone();
        invalid["id"] = serde_json::json!(format!("spl_{label}"));
        for (key, value) in patch.as_object().unwrap() {
            invalid[key] = value.clone();
        }
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version":1,
                "limits":[valid, invalid],
                "audit":[],
                "next_audit_id":1
            }))
            .unwrap(),
        )
        .unwrap();

        let (loaded, logs) = capture_logs(|| load(&path).unwrap().unwrap());
        assert_eq!(loaded.state.limits.len(), 1, "case {label}");
        assert_eq!(loaded.state.limits[0].id, "spl_valid", "case {label}");
        assert!(logs.contains(&format!("spl_{label}")), "{logs}");
        assert!(logs.contains(field), "{logs}");
        assert!(
            logs.contains("preserving invalid gateway spend-limit record"),
            "{logs}"
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

#[test]
fn malformed_typed_limit_is_hidden_without_losing_valid_limits() {
    let cases = [
        ("period", json!({"period":"quarterly"})),
        ("scope-type", json!({"scope":{"type":"workspace"}})),
    ];

    for (label, patch) in cases {
        let path = temp_file(label);
        let valid = json!({
            "id":"spl_valid",
            "amount":"100",
            "created_at":"2026-08-10T00:00:00.000Z",
            "currency":"USD",
            "period":"monthly",
            "scope":{"type":"organization"},
            "type":"spend_limit",
            "updated_at":"2026-08-10T00:00:00.000Z"
        });
        let mut malformed = valid.clone();
        malformed["id"] = json!(format!("spl_{label}"));
        for (key, value) in patch.as_object().unwrap() {
            malformed[key] = value.clone();
        }
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version":1,
                "limits":[valid, malformed],
                "audit":[],
                "next_audit_id":1
            }))
            .unwrap(),
        )
        .unwrap();

        let (loaded, logs) = capture_logs(|| load(&path).unwrap().unwrap());
        assert_eq!(loaded.state.limits.len(), 1, "case {label}");
        assert_eq!(loaded.state.limits[0].id, "spl_valid", "case {label}");
        assert!(
            logs.contains("preserving malformed gateway spend-limit record"),
            "{logs}"
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

#[test]
fn unknown_and_invalid_limits_round_trip_verbatim_without_becoming_visible() {
    let path = temp_file("unknown-roundtrip");
    let valid = json!({
        "id":"spl_valid",
        "amount":"100",
        "created_at":"2026-08-10T00:00:00.000Z",
        "currency":"USD",
        "period":"monthly",
        "scope":{"type":"organization"},
        "type":"spend_limit",
        "updated_at":"2026-08-10T00:00:00.000Z"
    });
    let unknown = json!({
        "id":"spl_future",
        "amount":"200",
        "created_at":"2026-08-10T00:00:00.000Z",
        "currency":"USD",
        "period":"monthly",
        "scope":{"type":"rbac_group","rbac_group_id":"eng"},
        "type":"spend_limit",
        "updated_at":"2026-08-10T00:00:00.000Z",
        "future_field":{"preserve":true}
    });
    let invalid = json!({
        "id":"spl_invalid",
        "amount":"300",
        "created_at":"2026-08-10T00:00:00.000Z",
        "currency":"EUR",
        "period":"monthly",
        "scope":{"type":"organization"},
        "type":"spend_limit",
        "updated_at":"2026-08-10T00:00:00.000Z"
    });
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,
            "limits":[valid, unknown.clone(), invalid.clone()],
            "audit":[],
            "next_audit_id":1
        }))
        .unwrap(),
    )
    .unwrap();

    let loaded = load(&path).unwrap().unwrap();
    assert_eq!(loaded.state.limits.len(), 1);
    assert_eq!(loaded.state.limits[0].id, "spl_valid");
    assert_eq!(
        loaded
            .state
            .opaque_limits
            .iter()
            .map(|record| record.value.clone())
            .collect::<Vec<_>>(),
        [unknown.clone(), invalid.clone()]
    );
    save_snapshot(&path, &loaded.state).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["limits"].as_array().unwrap().len(), 3);
    assert!(saved["limits"].as_array().unwrap().contains(&unknown));
    assert!(saved["limits"].as_array().unwrap().contains(&invalid));
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn valid_limit_with_unknown_field_round_trips_verbatim() {
    let path = temp_file("unknown-limit-field");
    let limit = json!({
        "id":"spl_future_field",
        "amount":"100",
        "created_at":"2026-08-10T00:00:00.000Z",
        "currency":"USD",
        "period":"monthly",
        "scope":{"type":"organization"},
        "type":"spend_limit",
        "updated_at":"2026-08-10T00:00:00.000Z",
        "future_policy":{"mode":"strict"}
    });
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,
            "limits":[limit.clone()],
            "audit":[],
            "next_audit_id":1
        }))
        .unwrap(),
    )
    .unwrap();

    let loaded = load(&path).unwrap().unwrap();
    save_snapshot(&path, &loaded.state).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["limits"], json!([limit]));
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn opaque_limit_keeps_its_position_between_visible_limits() {
    let path = temp_file("opaque-limit-order");
    let visible = |id: &str| {
        json!({
            "id":id,
            "amount":"100",
            "created_at":"2026-08-10T00:00:00.000Z",
            "currency":"USD",
            "period":"monthly",
            "scope":{"type":"organization"},
            "type":"spend_limit",
            "updated_at":"2026-08-10T00:00:00.000Z"
        })
    };
    let first = visible("spl_first");
    let opaque = json!({
        "id":"spl_future",
        "amount":"200",
        "created_at":"2026-08-10T00:00:00.000Z",
        "currency":"USD",
        "period":"monthly",
        "scope":{"type":"rbac_group","rbac_group_id":"eng"},
        "type":"spend_limit",
        "updated_at":"2026-08-10T00:00:00.000Z"
    });
    let last = visible("spl_last");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,
            "limits":[first, opaque, last],
            "audit":[],
            "next_audit_id":1
        }))
        .unwrap(),
    )
    .unwrap();

    let loaded = load(&path).unwrap().unwrap();
    save_snapshot(&path, &loaded.state).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let ids = saved["limits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["spl_first", "spl_future", "spl_last"]);
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn restore_tolerates_and_round_trips_unknown_audit_snapshots() {
    let _env_guard = crate::config::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_file("unknown-audit-snapshot");
    let audit = json!({
        "id":7,
        "created_at":"2026-08-10T00:00:00.000Z",
        "actor":"admin-key:test",
        "before":null,
        "after":{
            "id":"spl_future",
            "amount":"200",
            "created_at":"2026-08-10T00:00:00.000Z",
            "currency":"USD",
            "period":"monthly",
            "scope":{"type":"rbac_group","rbac_group_id":"eng"},
            "type":"spend_limit",
            "updated_at":"2026-08-10T00:00:00.000Z"
        }
    });
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,
            "limits":[],
            "audit":[audit.clone()],
            "next_audit_id":8
        }))
        .unwrap(),
    )
    .unwrap();

    let tokens_env = format!("SHUNT_SPEND_RESTORE_TOKENS_{}_audit", std::process::id());
    std::env::remove_var(&tokens_env);
    let state = app_state_with_path(path.clone(), &tokens_env);

    let runtime = tokio::runtime::Runtime::new().expect("build runtime");
    runtime
        .block_on(restore(&state))
        .expect("restore must tolerate future audit snapshots");
    save_snapshot(&path, &state.gateway_stores.spend.export()).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["audit"], json!([audit]));
    load(&path)
        .expect("saved audit state remains loadable")
        .expect("state exists");

    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn load_reconciles_next_audit_id_with_existing_records() {
    let path = temp_file("audit-id-reconcile");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,
            "limits":[],
            "audit":[{
                "id":41,
                "created_at":"2026-08-10T00:00:00.000Z",
                "actor":"admin-key:test",
                "before":null,
                "after":null
            }],
            "next_audit_id":3
        }))
        .unwrap(),
    )
    .unwrap();

    let loaded = load(&path).unwrap().unwrap();
    assert_eq!(loaded.state.next_audit_id, 42);
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn corrupt_and_version_mismatched_files_fail_closed() {
    let corrupt = temp_file("corrupt");
    fs::write(&corrupt, b"not json").unwrap();
    let error = load(&corrupt).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("not valid json"));
    fs::remove_dir_all(corrupt.parent().unwrap()).ok();

    let mismatch = temp_file("version");
    fs::write(
        &mismatch,
        br#"{"version":999,"limits":[],"audit":[],"next_audit_id":1}"#,
    )
    .unwrap();
    let error = load(&mismatch).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("version mismatch"));
    fs::remove_dir_all(mismatch.parent().unwrap()).ok();
}
