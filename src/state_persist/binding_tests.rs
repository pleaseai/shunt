//! Session bindings in the pool state file (ADR-0008 §6; FR-11, FR-12,
//! AC-017, NFR-3).

use std::path::PathBuf;

use serde::Deserialize;

use super::*;
use crate::config::{AccountConfig, AuthMode, Config, PoolConfig};

const PROVIDER: &str = "anthropic";
const MODEL: &str = "claude-sonnet-5-5";

fn temp_file(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-state-bindings-{}-{}-{label}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create test directory");
    dir.join("state.json")
}

fn remove_test_dir(path: &Path) {
    fs::remove_dir_all(path.parent().expect("test path has parent")).ok();
}

fn account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        uuid: Some(format!("uuid-{name}")),
        token_env: Some(format!("SHUNT_TEST_STATE_BINDING_{name}")),
        ..Default::default()
    }
}

/// A Claude pool over `names`, persisting to `path`.
fn pool_state(path: &Path, names: &[&str]) -> AppState {
    let mut config = Config::default();
    let provider = config.providers.get_mut(PROVIDER).unwrap();
    provider.auth = AuthMode::ClaudeOauth;
    provider.accounts = names.iter().map(|name| account(name)).collect();
    config.server.pool = Some(PoolConfig {
        state_path: Some(path.to_path_buf()),
        ..Default::default()
    });
    AppState::new(config, reqwest::Client::new()).expect("valid test config")
}

async fn accounts_of(state: &AppState) -> Vec<AccountConfig> {
    state
        .config
        .provider(PROVIDER)
        .unwrap()
        .resolve_pool_accounts()
        .await
        .expect("pool accounts resolve")
}

/// Select for `session` and commit `serve` (an index into `accounts`).
fn serve(state: &AppState, accounts: &[AccountConfig], session: &str, serve: usize) {
    let (_, ticket) = state.accounts.select_order_with_ticket(
        PROVIDER,
        accounts,
        Some(session),
        Some(MODEL),
        None,
    );
    state
        .accounts
        .commit_session_binding(&ticket.unwrap(), &accounts[serve], false);
}

/// The account name `session` is bound to in `state`, if any.
fn bound(state: &AppState, accounts: &[AccountConfig], session: &str) -> Option<String> {
    let (_, ticket) = state.accounts.select_order_with_ticket(
        PROVIDER,
        accounts,
        Some(session),
        Some(MODEL),
        None,
    );
    let ticket = ticket.unwrap();
    accounts
        .iter()
        .find(|account| ticket.is_bound_to(account))
        .map(|account| account.name.clone())
}

/// Whether the pool holds any binding for `session`, whatever its account.
fn has_binding(state: &AppState, accounts: &[AccountConfig], session: &str) -> bool {
    let (_, ticket) = state.accounts.select_order_with_ticket(
        PROVIDER,
        accounts,
        Some(session),
        Some(MODEL),
        None,
    );
    ticket.unwrap().is_bound()
}

/// AC-017 and FR-12: a restart restores every unexpired binding whose account
/// is still configured and drops the rest; the file never carries a raw
/// session id.
#[tokio::test]
async fn a_restart_restores_live_bindings_of_configured_accounts_only() {
    let path = temp_file("restart");
    let before = pool_state(&path, &["a", "b"]);
    let accounts = accounts_of(&before).await;
    let kept = "raw-session-kept-7f3a";
    let removed = "raw-session-removed-91c2";
    serve(&before, &accounts, kept, 0);
    serve(&before, &accounts, removed, 1);
    flush(&before).await;

    let raw = fs::read_to_string(&path).expect("state file written");
    assert!(raw.contains("\"bindings\""), "{raw}");
    assert!(!raw.contains(kept) && !raw.contains(removed), "{raw}");

    // An expired row next to the live ones.
    let mut file = load(&path).unwrap().unwrap();
    let live = &file.bindings[0];
    file.bindings.push(PersistedBinding {
        provider: PROVIDER.to_string(),
        session_hash: "ab".repeat(32),
        account: live.account.clone(),
        last_used: 1_000,
    });
    save(&path, &file).unwrap();

    // Restart with account-b gone from the configuration.
    let after = pool_state(&path, &["a"]);
    restore(&after).await;
    let remaining = accounts_of(&after).await;
    assert_eq!(bound(&after, &remaining, kept).as_deref(), Some("a"));
    assert_eq!(bound(&after, &remaining, removed), None);
    assert!(
        !has_binding(&after, &remaining, removed),
        "no binding survives for the removed account"
    );
    assert_eq!(
        after.accounts.export_bindings().len(),
        1,
        "only the live row"
    );
    assert!(
        after.accounts.take_dirty(),
        "dropped rows mark the pool dirty so the file is rewritten"
    );
    remove_test_dir(&path);
}

/// `import_bindings` raises `any_bindings`: the first selection after a
/// restore follows a restored binding even though placement alone would pick
/// another account.
#[tokio::test]
async fn the_first_selection_after_restore_follows_the_restored_binding() {
    let path = temp_file("first-selection");
    let before = pool_state(&path, &["a", "b", "c"]);
    let accounts = accounts_of(&before).await;
    let session = "restored-session";
    let (order, _) = before.accounts.select_order_with_ticket(
        PROVIDER,
        &accounts,
        Some(session),
        Some(MODEL),
        None,
    );
    // Bind the session to an account that is not its first choice.
    let other = order[1];
    serve(&before, &accounts, session, other);
    flush(&before).await;

    let after = pool_state(&path, &["a", "b", "c"]);
    restore(&after).await;
    let order = after
        .accounts
        .select_order(PROVIDER, &accounts, Some(session), Some(MODEL), None);
    assert_eq!(order[0], other, "the restored binding leads the order");
    remove_test_dir(&path);
}

/// A file without `bindings` (v3 before bindings, or v2) loads with none,
/// and a pool with no bindings writes no `bindings` key.
#[tokio::test]
async fn a_file_without_bindings_loads_with_none() {
    for (label, version) in [("v3", STATE_VERSION), ("v2", LEGACY_STATE_VERSION)] {
        let path = temp_file(label);
        fs::write(&path, format!(r#"{{"version":{version},"accounts":[]}}"#)).unwrap();
        let state = pool_state(&path, &["a", "b"]);
        restore(&state).await;
        assert!(state.accounts.export_bindings().is_empty(), "{label}");
        let accounts = accounts_of(&state).await;
        assert_eq!(bound(&state, &accounts, "any-session"), None, "{label}");

        state.accounts.mark_dirty();
        flush(&state).await;
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("bindings"), "{label}: {raw}");
        remove_test_dir(&path);
    }
}

/// NFR-3: the previous release's envelope, which has no `bindings` field,
/// reads a file written with bindings and keeps its quota.
#[tokio::test]
async fn the_previous_release_reads_a_file_with_bindings() {
    /// `PersistedPool` as the release before bindings declared it.
    #[derive(Deserialize)]
    struct PreviousPersistedPool {
        version: u32,
        accounts: Vec<PersistedAccount>,
    }

    let path = temp_file("previous-release");
    let state = pool_state(&path, &["a", "b"]);
    let accounts = accounts_of(&state).await;
    serve(&state, &accounts, "downgrade-session", 0);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        reqwest::header::HeaderValue::from_static("0.25"),
    );
    state.accounts.note_quota(PROVIDER, &accounts[0], &headers);
    flush(&state).await;

    let bytes = fs::read(&path).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("\"bindings\""));
    let previous: PreviousPersistedPool =
        serde_json::from_slice(&bytes).expect("the previous release parses the file");
    assert_eq!(previous.version, STATE_VERSION, "still v3");
    assert_eq!(previous.accounts.len(), 1);
    assert_eq!(previous.accounts[0].quota.utilization_5h, Some(0.25));
    remove_test_dir(&path);
}

/// Persisted `last_used` advances at most hourly: it is written rounded down
/// to the hour, and a serve that leaves the hour unchanged does not mark the
/// pool dirty.
#[tokio::test]
async fn persisted_last_used_is_hourly_and_a_same_hour_touch_writes_nothing() {
    let path = temp_file("hourly");
    let state = pool_state(&path, &["a", "b"]);
    let accounts = accounts_of(&state).await;
    let hour = || crate::accounts::persisted_last_used_for_tests(now());
    let started = hour();
    serve(&state, &accounts, "hourly-session", 0);
    assert!(
        state.accounts.take_dirty(),
        "a placement marks the pool dirty"
    );
    for row in state.accounts.export_bindings() {
        assert_eq!(row.last_used % 3_600, 0, "rounded to the hour");
    }
    serve(&state, &accounts, "hourly-session", 0);
    // Only meaningful when the clock did not cross an hour mid-test.
    if hour() == started {
        assert!(
            !state.accounts.take_dirty(),
            "a touch within the same hour leaves the file alone"
        );
    }
    remove_test_dir(&path);
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
