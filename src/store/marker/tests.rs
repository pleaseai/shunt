//! The sidecar's pure decision and its log output.

use super::*;
use crate::store::test_support::{capture_logs, TempDir};

fn marker_for(snapshot: &[u8]) -> ImportMarker {
    ImportMarker {
        backend: "sqlite".to_string(),
        imported_at: 1_700_000_123,
        source_sha256: sha256_hex(snapshot),
    }
}

#[test]
fn sidecar_sits_beside_the_snapshot() {
    assert_eq!(
        sidecar_path(Path::new("/var/lib/shunt/pool.json")),
        PathBuf::from("/var/lib/shunt/pool.json.imported")
    );
}

#[test]
fn sha256_matches_the_known_digest() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn marker_applies_only_to_the_bytes_it_was_written_for() {
    let sidecar = serde_json::to_vec(&marker_for(b"v1")).unwrap();
    assert_eq!(
        current_marker(&sidecar, b"v1").map(|m| m.imported_at),
        Some(1_700_000_123)
    );
    assert_eq!(current_marker(&sidecar, b"v2"), None, "changed snapshot");
    assert_eq!(
        current_marker(b"not json", b"v1"),
        None,
        "unreadable sidecar"
    );
}

#[test]
fn warns_while_the_snapshot_is_unchanged_and_goes_silent_when_it_changes() {
    let dir = TempDir::new("marker");
    let snapshot = dir.file("state.json");
    std::fs::write(&snapshot, b"v1").unwrap();
    write(&snapshot, &marker_for(b"v1")).unwrap();

    let logs = capture_logs(|| warn_if_imported("pool", &snapshot, b"v1"));
    assert!(logs.contains("imported into a store"), "{logs}");
    assert!(logs.contains("1700000123"), "{logs}");
    assert!(logs.contains("state.json"), "{logs}");

    let logs = capture_logs(|| warn_if_imported("pool", &snapshot, b"v2"));
    assert!(logs.is_empty(), "snapshot changed since import: {logs}");
}

#[test]
fn silent_without_a_sidecar() {
    let dir = TempDir::new("marker-none");
    let logs = capture_logs(|| warn_if_imported("pool", &dir.file("state.json"), b"v1"));
    assert!(logs.is_empty(), "{logs}");
}

#[cfg(unix)]
#[test]
fn sidecar_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("marker-mode");
    let snapshot = dir.file("state.json");
    write(&snapshot, &marker_for(b"v1")).unwrap();
    let mode = std::fs::metadata(sidecar_path(&snapshot))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
