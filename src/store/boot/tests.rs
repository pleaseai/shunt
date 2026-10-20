use std::path::{Path, PathBuf};
use std::time::Duration;

use super::start;
use crate::config::{Config, PoolConfig};
use crate::store::test_support::{sqlite_config, TempDir};
use crate::store::{Mode, Store, StoreError};

/// Whether one source line names this module (`crate::store`) rather than
/// one of the other modules called `store` (`gateway::store`,
/// `claude::store`, `routing::stage::store`, `spend::store`, ...).
fn names_the_store_module(line: &str) -> bool {
    let path = ["crate::store", "shunt::store"].iter().any(|needle| {
        line.match_indices(needle).any(|(at, _)| {
            let before = line[..at].chars().next_back();
            let after = line[at + needle.len()..].chars().next();
            let starts_path =
                !matches!(before, Some(c) if c.is_alphanumeric() || c == '_' || c == ':');
            let ends_ident = !matches!(after, Some(c) if c.is_alphanumeric() || c == '_');
            starts_path && ends_ident
        })
    });
    // `use crate::{store::Store, ...}` on one line.
    let grouped = (line.contains("crate::{") || line.contains("shunt::{"))
        && [
            "{store::",
            "{store,",
            "{store}",
            ", store::",
            ", store,",
            ", store}",
        ]
        .iter()
        .any(|form| line.contains(form));
    path || grouped
}

/// `GatewayStores::store()` hands out the attached store without naming the
/// module, so a path scan misses it. Empty parentheses tell it apart from
/// `AtomicBool::store(value, ordering)`; a chain split across lines puts
/// `.store()` on a line of its own, which this still sees.
fn calls_the_store_accessor(line: &str) -> bool {
    line.contains(".store()")
}

/// Offending `(line number, line)` pairs in one file. Inside a multi-line
/// `use crate::{ ... }` group, a line that starts with `store` is the store
/// module too (a nested `gateway::{\n store::X` also matches: a false
/// positive fails loudly, which is the safe direction).
fn offending_lines(text: &str, root_level: bool) -> Vec<(usize, String)> {
    let mut offenders = Vec::new();
    let mut in_group = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let group_member = in_group
            && (trimmed.starts_with("store::")
                || trimmed.starts_with("store,")
                || trimmed == "store");
        // `src/proxy.rs` sits at the crate root, so `super::store` is the
        // store module there too.
        let relative = root_level && line.contains("super::store");
        if names_the_store_module(line)
            || calls_the_store_accessor(line)
            || group_member
            || relative
        {
            offenders.push((index + 1, line.to_string()));
        }
        if line.contains("crate::{") || line.contains("shunt::{") {
            in_group = !line.contains("};");
        } else if in_group && line.contains("};") {
            in_group = false;
        }
    }
    offenders
}

fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        if root.extension().is_some_and(|ext| ext == "rs") {
            out.push(root.to_path_buf());
        }
        return;
    }
    for entry in std::fs::read_dir(root).expect("read source directory") {
        rust_files(&entry.expect("directory entry").path(), out);
    }
}

#[test]
fn the_matcher_flags_this_module_and_only_this_module() {
    for flagged in [
        "use crate::store::Store;",
        "use crate::store;",
        "    let s: &crate::store::Store = x;",
        "use crate::{server, store::Store};",
        "use crate::{store::Store, server};",
        "shunt::store::boot::start(&c, &s)",
    ] {
        assert!(names_the_store_module(flagged), "{flagged}");
    }
    for clean in [
        "use crate::gateway::store::GatewayStores;",
        "use crate::auth::claude::store;",
        "use crate::routing::stage::store::StageStore;",
        "use crate::store_x::Thing;",
        "use crate::{gateway::store::GatewayStores, server};",
        "ready.store(true, Ordering::SeqCst);",
    ] {
        assert!(!names_the_store_module(clean), "{clean}");
        assert!(!calls_the_store_accessor(clean), "{clean}");
    }
    for accessor in [
        "let store = state.gateway_stores.store();",
        "        .store()",
    ] {
        assert!(calls_the_store_accessor(accessor), "{accessor}");
        assert_eq!(offending_lines(accessor, false).len(), 1, "{accessor}");
    }
    let multi_line = "use crate::{\n    server::AppState,\n    store::Store,\n};\nfn f() {}\n";
    assert_eq!(offending_lines(multi_line, false).len(), 1);
    assert_eq!(offending_lines("use super::store::Store;", true).len(), 1);
    assert!(offending_lines("use super::store::Store;", false).is_empty());
}

/// AC-013 / NFR-001: the request path never reaches the store, so a slow or
/// unavailable store cannot slow or fail a proxied request.
#[test]
fn nothing_on_the_proxy_path_references_the_store_module() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&src).expect("read src") {
        let path = entry.expect("src entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("proxy") || name == "adapters" {
            rust_files(&path, &mut files);
        }
    }
    assert!(
        files.iter().any(|f| f.ends_with("proxy.rs")) && files.len() > 20,
        "the scan must cover src/proxy* and src/adapters/**; found {files:?}"
    );
    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read source file");
        let root_level = file.parent() == Some(src.as_path());
        for (number, line) in offending_lines(&text, root_level) {
            offenders.push(format!("{}:{number}: {line}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "the proxy path must not reference crate::store:\n{}",
        offenders.join("\n")
    );
}

#[tokio::test]
async fn no_store_section_means_no_runtime() {
    let config = Config::default();
    let (_router, _shared, state) = crate::server::build_router(config.clone()).unwrap();
    assert!(start(&config, &state).await.unwrap().is_none());
    assert!(state.gateway_stores.store().is_none());
}

/// A failure after the open (here: a malformed pool snapshot) releases the
/// hold, so a corrected restart opens at once instead of waiting out the
/// stale window.
#[tokio::test]
async fn a_failed_start_releases_the_hold() {
    let dir = TempDir::new("boot-fail");
    let snapshot = dir.file("pool.json");
    std::fs::write(&snapshot, b"{not json").unwrap();
    let mut config = Config::default();
    config.server.pool = Some(PoolConfig {
        state_path: Some(snapshot.clone()),
        ..PoolConfig::default()
    });
    let store_config = sqlite_config(&dir.file("store.db"));
    config.store = Some(Box::new(store_config.clone()));
    let (_router, _shared, state) = crate::server::build_router(config.clone()).unwrap();

    let error = match start(&config, &state).await {
        Err(error) => error,
        Ok(_) => panic!("a malformed snapshot must fail the boot"),
    };
    assert!(
        matches!(
            error,
            StoreError::Import {
                subsystem: "pool",
                ..
            }
        ),
        "{error}"
    );
    let reopened = Store::open(&store_config, Mode::Serve)
        .await
        .expect("the failed boot released its hold");
    reopened.close().await;
}

/// Shutdown releases the hold: an immediate second open succeeds where it
/// would otherwise be refused as `HoldHeld` for `hold_stale_seconds`.
#[tokio::test]
async fn shutdown_releases_the_hold() {
    let dir = TempDir::new("boot-shutdown");
    let mut config = Config::default();
    let store_config = sqlite_config(&dir.file("store.db"));
    config.store = Some(Box::new(store_config.clone()));
    let (_router, _shared, state) = crate::server::build_router(config.clone()).unwrap();
    let runtime = start(&config, &state).await.unwrap().expect("a runtime");
    assert!(state.gateway_stores.store().is_some());

    let held = Store::open(&store_config, Mode::Serve).await;
    assert!(matches!(held, Err(StoreError::HoldHeld { .. })), "{held:?}");

    runtime.shutdown(Duration::from_secs(5)).await;
    let reopened = Store::open(&store_config, Mode::Serve)
        .await
        .expect("shutdown released the hold");
    reopened.close().await;
}

/// An import that outlasts `hold_stale_seconds` keeps the hold: the heartbeat
/// already runs while the snapshot read blocks (a FIFO with no writer yet), so
/// a second opener past the stale window is still refused.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hold_is_renewed_while_the_imports_run() {
    let dir = TempDir::new("boot-slow-import");
    let snapshot = dir.file("pool.json");
    let fifo = std::ffi::CString::new(snapshot.to_str().unwrap()).unwrap();
    // SAFETY: `fifo` is a valid NUL-terminated path for the whole call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut config = Config::default();
    config.server.pool = Some(PoolConfig {
        state_path: Some(snapshot.clone()),
        ..PoolConfig::default()
    });
    let mut store_config = sqlite_config(&dir.file("store.db"));
    store_config.hold_stale_seconds = crate::store::config::MIN_HOLD_STALE_SECONDS;
    config.store = Some(Box::new(store_config.clone()));
    let (_router, _shared, state) = crate::server::build_router(config.clone()).unwrap();
    let boot = tokio::spawn(async move { start(&config, &state).await });

    tokio::time::sleep(Duration::from_secs(store_config.hold_stale_seconds + 2)).await;
    let rival = Store::open(&store_config, Mode::Serve).await;
    // Unblock the read before asserting, so a failure ends the test instead
    // of leaving the boot parked on the FIFO.
    std::fs::write(&snapshot, br#"{"version":3,"accounts":[]}"#).unwrap();
    let booted = boot.await.unwrap();
    assert!(
        matches!(rival, Err(StoreError::HoldHeld { .. })),
        "{rival:?}"
    );
    let runtime = booted
        .unwrap()
        .expect("the boot finishes once the snapshot is readable");
    runtime.shutdown(Duration::from_secs(5)).await;
}
