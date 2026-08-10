use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use super::store::{validate_limit, AuditRecord, SpendLimit, SpendState};
use crate::server::AppState;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSpend {
    version: u32,
    #[serde(flatten)]
    state: SpendState,
}

#[derive(Deserialize)]
struct PersistedSpendWire {
    version: u32,
    limits: Vec<serde_json::Value>,
    audit: Vec<AuditRecord>,
    next_audit_id: u64,
}

pub async fn restore(state: &AppState) -> io::Result<()> {
    let Some(path) = state
        .gateway_stores
        .spend
        .state_path()
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    let load_path = path.clone();
    match tokio::task::spawn_blocking(move || load(&load_path)).await {
        Ok(Ok(Some(persisted))) => {
            let limits = persisted.state.limits.len();
            let audit_records = persisted.state.audit.len();
            state.gateway_stores.spend.replace(persisted.state);
            tracing::info!(
                path = %path.display(),
                limits,
                audit_records,
                "restored gateway spend-limit state from disk"
            );
            Ok(())
        }
        Ok(Ok(None)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(io::Error::other(format!(
            "gateway spend-limit restore task panicked: {error}"
        ))),
    }
}

pub(crate) async fn save(state: &AppState, snapshot: &SpendState) -> Result<(), String> {
    let Some(path) = state
        .gateway_stores
        .spend
        .state_path()
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || save_snapshot(&path, &snapshot))
        .await
        .map_err(|error| format!("gateway spend-limit persistence task panicked: {error}"))?
        .map_err(|error| format!("failed to persist gateway spend-limit state: {error}"))
}

fn load(path: &Path) -> io::Result<Option<PersistedSpend>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let wire: PersistedSpendWire = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("gateway spend-limit state is not valid json: {error}"),
        )
    })?;
    if wire.version != STATE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "gateway spend-limit state version mismatch: found {}, expected {STATE_VERSION}",
                wire.version
            ),
        ));
    }
    let limits = wire
        .limits
        .into_iter()
        .filter_map(|value| match serde_json::from_value::<SpendLimit>(value) {
            Ok(limit) => match validate_limit(&limit) {
                Ok(()) => Some(limit),
                Err(error) => {
                    tracing::warn!(
                        id = %limit.id,
                        field = error.field(),
                        "dropping invalid gateway spend-limit record from persisted state"
                    );
                    None
                }
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "dropping malformed gateway spend-limit record from persisted state"
                );
                None
            }
        })
        .collect();
    Ok(Some(PersistedSpend {
        version: wire.version,
        state: SpendState {
            limits,
            audit: wire.audit,
            next_audit_id: wire.next_audit_id,
        },
    }))
}

fn save_snapshot(path: &Path, state: &SpendState) -> io::Result<()> {
    let persisted = PersistedSpend {
        version: STATE_VERSION,
        state: state.clone(),
    };
    let json = serde_json::to_vec_pretty(&persisted).map_err(io::Error::other)?;
    crate::atomic_file::write_private_atomic(path, &json)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::gateway::spend::store::{Period, Scope, SpendStore};

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

    #[tokio::test]
    async fn app_state_restore_populates_the_process_lifetime_store() {
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

        let suffix = format!("{}_restore", std::process::id());
        let secret_env = format!("SHUNT_SPEND_RESTORE_SECRET_{suffix}");
        let users_env = format!("SHUNT_SPEND_RESTORE_USERS_{suffix}");
        let write_env = format!("SHUNT_SPEND_RESTORE_WRITE_{suffix}");
        let read_env = format!("SHUNT_SPEND_RESTORE_READ_{suffix}");
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&users_env, "dev@example.com:password");
        let mut config = crate::config::Config::default();
        config.server.gateway = Some(crate::config::GatewayConfig {
            public_url: "https://gateway.example".into(),
            jwt_secret_env: secret_env.clone(),
            users_env: users_env.clone(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            admin: Some(crate::config::GatewayAdminConfig {
                write_keys_env: write_env.clone(),
                read_keys_env: read_env.clone(),
                blocked_message: None,
                audit_retention_days: 365,
                spend_retention_months: 13,
                identity_retention_days: 90,
                group_limit_mode: crate::config::GroupLimitMode::Min,
                state_path: Some(path.clone()),
                write_keys: Vec::new(),
                read_keys: Vec::new(),
            }),
            enforcement: crate::config::GatewayEnforcementConfig::default(),
            oidc: None,
        });
        let state =
            crate::server::AppState::new(config, reqwest::Client::new()).expect("build app state");
        restore(&state).await.expect("restore state");

        assert_eq!(state.gateway_stores.spend.get(&expected.id), Some(expected));
        assert_eq!(state.gateway_stores.spend.export().audit.len(), 1);
        for env in [secret_env, users_env, write_env, read_env] {
            std::env::remove_var(env);
        }
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn invalid_persisted_limits_are_dropped_individually_and_warned() {
        let cases = [
            ("amount-negative", "amount", json!({"amount":"-5"})),
            ("amount-text", "amount", json!({"amount":"abc"})),
            (
                "amount-long",
                "amount",
                json!({"amount":"9".repeat(super::super::store::MAX_AMOUNT_LENGTH + 1)}),
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
                logs.contains("dropping invalid gateway spend-limit record"),
                "{logs}"
            );
            fs::remove_dir_all(path.parent().unwrap()).ok();
        }
    }

    #[test]
    fn malformed_typed_limit_is_dropped_without_losing_valid_limits() {
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
                logs.contains("dropping malformed gateway spend-limit record"),
                "{logs}"
            );
            fs::remove_dir_all(path.parent().unwrap()).ok();
        }
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
}
