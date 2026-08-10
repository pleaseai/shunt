use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use super::store::SpendState;
use crate::server::AppState;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSpend {
    version: u32,
    #[serde(flatten)]
    state: SpendState,
}

pub async fn restore(state: &AppState) {
    let Some(path) = state
        .gateway_stores
        .spend
        .state_path()
        .map(ToOwned::to_owned)
    else {
        return;
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
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to read gateway spend-limit state; starting empty"
        ),
        Err(error) => tracing::warn!(%error, "gateway spend-limit restore task panicked"),
    }
}

pub(crate) async fn save(state: &AppState) -> Result<(), String> {
    let Some(path) = state
        .gateway_stores
        .spend
        .state_path()
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    let snapshot = state.gateway_stores.spend.export();
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
    let persisted: PersistedSpend = match serde_json::from_slice(&bytes) {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "gateway spend-limit state is not valid json; ignoring");
            return Ok(None);
        }
    };
    if persisted.version != STATE_VERSION {
        tracing::warn!(
            path = %path.display(),
            found = persisted.version,
            expected = STATE_VERSION,
            "gateway spend-limit state version mismatch; ignoring"
        );
        return Ok(None);
    }
    Ok(Some(persisted))
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
        restore(&state).await;

        assert_eq!(state.gateway_stores.spend.get(&expected.id), Some(expected));
        assert_eq!(state.gateway_stores.spend.export().audit.len(), 1);
        for env in [secret_env, users_env, write_env, read_env] {
            std::env::remove_var(env);
        }
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_and_version_mismatched_files_are_ignored() {
        let corrupt = temp_file("corrupt");
        fs::write(&corrupt, b"not json").unwrap();
        assert!(load(&corrupt).unwrap().is_none());
        fs::remove_dir_all(corrupt.parent().unwrap()).ok();

        let mismatch = temp_file("version");
        fs::write(
            &mismatch,
            br#"{"version":999,"limits":[],"audit":[],"next_audit_id":1}"#,
        )
        .unwrap();
        assert!(load(&mismatch).unwrap().is_none());
        fs::remove_dir_all(mismatch.parent().unwrap()).ok();
    }
}
