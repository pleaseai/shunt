use super::*;
use crate::accounts::{AccountStateIdentity, StoreFamily};

fn acct(name: &str) -> AccountKey {
    AccountKey {
        store_family: StoreFamily::Claude,
        identity: AccountStateIdentity::StoreEntry {
            name: name.to_string(),
        },
    }
}

fn h(key: &str) -> SessionHash {
    hash_session_key(key)
}

#[test]
fn key_is_sha256_and_raw_id_never_exported_or_printed() {
    let raw = "raw-session-id-ABC123";
    let mut t = SessionBindings::default();
    t.commit("claude", &h(raw), &acct("a"), 0, 1000);
    let expected = to_hex(&Sha256::digest(raw.as_bytes()));
    let rows = t.export(1000);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_hash, expected);
    assert!(!format!("{rows:?}{t:?}").contains(raw));
    assert_eq!(from_hex(&expected), Some(h(raw)));
}

#[test]
fn hashing_is_total_over_odd_inputs() {
    let long = "x".repeat(10 * 1024 * 1024);
    for k in ["", "\u{0}", "\u{fffd}\u{1f600}", long.as_str()] {
        let mut t = SessionBindings::default();
        t.commit("p", &h(k), &acct("a"), 0, 1);
        assert_eq!(t.len(), 1);
    }
}

#[test]
fn placement_touch_move_and_stale() {
    let mut t = SessionBindings::default();
    let s = h("s");
    assert_eq!(t.commit("p", &s, &acct("a"), 0, 10), CommitOutcome::Placed);
    let v = t.lookup("p", &s, 11).unwrap();
    assert_eq!(v.account, acct("a"));
    assert_eq!(
        t.commit("p", &s, &acct("a"), v.generation, 20),
        CommitOutcome::Touched
    );
    assert_eq!(
        t.commit("p", &s, &acct("b"), v.generation, 30),
        CommitOutcome::Moved { from: acct("a") }
    );
    let v2 = t.lookup("p", &s, 31).unwrap();
    assert_eq!(v2.account, acct("b"));
    assert_eq!(v2.generation, v.generation + 1);
    // A commit observed before the move must not overwrite it.
    assert_eq!(
        t.commit("p", &s, &acct("c"), v.generation, 40),
        CommitOutcome::Stale
    );
    assert_eq!(t.lookup("p", &s, 41).unwrap().account, acct("b"));
    // Other providers and sessions are independent.
    assert!(t.lookup("q", &s, 41).is_none());
    assert!(t.lookup("p", &h("other"), 41).is_none());
}

#[test]
fn idle_entry_expires_after_24h() {
    let mut t = SessionBindings::default();
    let s = h("s");
    t.commit("p", &s, &acct("a"), 0, 1000);
    assert!(t.lookup("p", &s, 1000 + BINDING_IDLE_TTL_SECS).is_some());
    assert!(t.lookup("p", &s, 1001 + BINDING_IDLE_TTL_SECS).is_none());
    assert_eq!(t.len(), 0);
}

#[test]
fn touch_extends_ttl() {
    let mut t = SessionBindings::default();
    let s = h("s");
    t.commit("p", &s, &acct("a"), 0, 1000);
    t.commit("p", &s, &acct("a"), 1, 1000 + BINDING_IDLE_TTL_SECS);
    assert!(t
        .lookup("p", &s, 1000 + 2 * BINDING_IDLE_TTL_SECS)
        .is_some());
}

#[test]
fn full_table_evicts_lru_in_a_batch() {
    let mut t = SessionBindings::default();
    for i in 0..MAX_BINDINGS {
        t.commit("p", &h(&format!("s{i}")), &acct("a"), 0, 1000 + i as u64);
    }
    assert_eq!(t.len(), MAX_BINDINGS);
    assert_eq!(t.evictions(), 0);
    t.commit("p", &h("new"), &acct("a"), 0, 50_000);
    assert!(t.evictions() >= 1);
    // Batch: back to ~90 %, so the next inserts evict nothing more.
    let after = t.evictions();
    assert!(t.len() <= MAX_BINDINGS * 9 / 10 + 1);
    for i in 0..100 {
        t.commit("p", &h(&format!("n{i}")), &acct("a"), 0, 50_001 + i);
    }
    assert_eq!(t.evictions(), after);
    // Oldest gone, newest kept.
    assert!(t.lookup("p", &h("s0"), 50_200).is_none());
    assert!(t.lookup("p", &h("new"), 50_200).is_some());
    assert!(t.len() <= MAX_BINDINGS);
}

#[test]
fn remove_account_and_bound_count() {
    let mut t = SessionBindings::default();
    t.commit("p", &h("1"), &acct("a"), 0, 10);
    t.commit("p", &h("2"), &acct("a"), 0, 10);
    t.commit("q", &h("3"), &acct("b"), 0, 10);
    assert_eq!(t.bound_count(&acct("a"), 11), 2);
    assert_eq!(t.bound_count(&acct("a"), 10 + BINDING_IDLE_TTL_SECS + 1), 0);
    assert_eq!(t.remove_matching(|k| *k == acct("a")), 2);
    assert_eq!(t.len(), 1);
    assert_eq!(t.bound_count(&acct("b"), 11), 1);
}

#[test]
fn export_import_round_trip_drops_expired_and_clamps_future() {
    let now = 1_000_000;
    let mut src = SessionBindings::default();
    src.commit("p", &h("fresh"), &acct("a"), 0, now - 10);
    let mut rows = src.export(now);
    rows.push(BindingRow {
        provider: "p".into(),
        session_hash: to_hex(&h("old")),
        account: acct("a"),
        last_used: now - BINDING_IDLE_TTL_SECS - 1,
    });
    rows.push(BindingRow {
        provider: "p".into(),
        session_hash: to_hex(&h("future")),
        account: acct("b"),
        last_used: now + 999_999,
    });
    rows.push(BindingRow {
        provider: "p".into(),
        session_hash: "not-hex".into(),
        account: acct("b"),
        last_used: now,
    });
    let mut t = SessionBindings::default();
    t.import(rows, now);
    assert_eq!(t.len(), 2);
    assert!(t.lookup("p", &h("fresh"), now).is_some());
    assert!(t.lookup("p", &h("old"), now).is_none());
    // Clamped to now: still live a TTL later minus a second, gone after.
    assert!(t
        .lookup("p", &h("future"), now + BINDING_IDLE_TTL_SECS)
        .is_some());
    assert!(t
        .lookup("p", &h("future"), now + BINDING_IDLE_TTL_SECS + 1)
        .is_none());
}

mod thinking_bound_predicate {
    use super::super::thinking_bound;
    use crate::config::AuthMode;
    use crate::routing::AdapterKind;

    fn claude(model: &str) -> bool {
        thinking_bound(&AdapterKind::Anthropic, AuthMode::ClaudeOauth, model)
    }

    #[test]
    fn sonnet_5_5_is_thinking_bound() {
        assert!(claude("claude-sonnet-5-5"));
        assert!(claude("claude-sonnet-5-5-20261001"));
    }

    #[test]
    fn haiku_5_5_is_thinking_bound() {
        assert!(claude("claude-haiku-5-5"));
        assert!(claude("claude-haiku-5-5-20261001"));
    }

    #[test]
    fn other_claude_families_are_not_thinking_bound() {
        for model in [
            "claude-opus-5-5",
            "claude-fable-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-sonnet-5",
        ] {
            assert!(!claude(model), "{model}");
        }
    }

    #[test]
    fn kimi_is_not_thinking_bound() {
        assert!(!thinking_bound(
            &AdapterKind::Anthropic,
            AuthMode::KimiOauth,
            "kimi-for-coding"
        ));
        // The predicate keys on the pool, not on a model name it was handed.
        assert!(!thinking_bound(
            &AdapterKind::Anthropic,
            AuthMode::KimiOauth,
            "claude-sonnet-5-5"
        ));
    }

    #[test]
    fn responses_and_codex_are_not_thinking_bound() {
        for auth in [AuthMode::ChatgptOauth, AuthMode::ChatgptPlanOauth] {
            assert!(!thinking_bound(
                &AdapterKind::Responses,
                auth,
                "gpt-5.5-codex"
            ));
            assert!(!thinking_bound(
                &AdapterKind::Responses,
                auth,
                "claude-sonnet-5-5"
            ));
        }
    }

    #[test]
    fn xai_is_not_thinking_bound() {
        assert!(!thinking_bound(
            &AdapterKind::Responses,
            AuthMode::XaiOauth,
            "grok-5"
        ));
        assert!(!thinking_bound(
            &AdapterKind::Responses,
            AuthMode::XaiOauth,
            "claude-haiku-5-5"
        ));
    }
}

#[test]
fn persisted_view_changes_on_placement_move_and_hour_crossing_only() {
    let hour = PERSISTED_LAST_USED_SECS;
    let start = 1_000 * hour + 10;
    let mut t = SessionBindings::default();
    assert_eq!(
        t.commit("p", &h("s"), &acct("a"), 0, start),
        CommitOutcome::Placed
    );
    assert!(
        t.take_persist_dirty(),
        "a placement changes the persisted view"
    );
    assert!(!t.take_persist_dirty(), "taking clears it");
    assert_eq!(t.export(start)[0].last_used, 1_000 * hour);

    // Touches within the same hour leave the persisted view alone.
    for later in [start + 1, start + hour - 11] {
        assert_eq!(
            t.commit("p", &h("s"), &acct("a"), 1, later),
            CommitOutcome::Touched
        );
        assert!(!t.take_persist_dirty(), "touch at +{}", later - start);
    }
    assert_eq!(t.export(start + hour - 11)[0].last_used, 1_000 * hour);

    // Crossing into the next hour does change it.
    let next = 1_001 * hour;
    assert_eq!(
        t.commit("p", &h("s"), &acct("a"), 1, next),
        CommitOutcome::Touched
    );
    assert!(t.take_persist_dirty());
    assert_eq!(t.export(next)[0].last_used, next);

    assert!(matches!(
        t.commit("p", &h("s"), &acct("b"), 1, next + 1),
        CommitOutcome::Moved { .. }
    ));
    assert!(t.take_persist_dirty(), "a move changes it");
    assert_eq!(t.remove_matching(|k| *k == acct("b")), 1);
    assert!(t.take_persist_dirty(), "a removal changes it");
}

#[test]
fn bound_counts_are_per_provider_and_skip_expired_bindings() {
    let mut t = SessionBindings::default();
    t.commit("p", &h("1"), &acct("a"), 0, 10);
    t.commit("p", &h("2"), &acct("a"), 0, 10);
    t.commit("p", &h("3"), &acct("b"), 0, 10);
    t.commit("q", &h("4"), &acct("a"), 0, 10);
    t.commit("p", &h("old"), &acct("b"), 0, 1);
    // `old` is idle past the TTL at `now`; the rest are not.
    let now = 1 + BINDING_IDLE_TTL_SECS + 1;
    let counts = t.bound_counts(now);
    assert_eq!(counts["p"][&acct("a")], 2);
    assert_eq!(
        counts["p"][&acct("b")],
        1,
        "the expired binding is not counted"
    );
    assert_eq!(counts["q"][&acct("a")], 1);
    assert_eq!(counts.len(), 2);
    assert!(t.bound_counts(10 + BINDING_IDLE_TTL_SECS + 1).is_empty());
}

#[test]
fn a_ticket_from_before_removal_cannot_overwrite_a_replaced_binding() {
    let s = h("s");
    // Removal by account (forget_identity).
    let mut t = SessionBindings::default();
    t.commit("p", &s, &acct("a"), 0, 10);
    let old = t.lookup("p", &s, 11).unwrap().generation;
    t.remove_matching(|a| *a == acct("a"));
    assert_eq!(t.commit("p", &s, &acct("b"), 0, 12), CommitOutcome::Placed);
    assert_eq!(t.commit("p", &s, &acct("c"), old, 13), CommitOutcome::Stale);
    assert_eq!(t.lookup("p", &s, 14).unwrap().account, acct("b"));

    // Removal by idle expiry.
    let mut t = SessionBindings::default();
    t.commit("p", &s, &acct("a"), 0, 10);
    let old = t.lookup("p", &s, 11).unwrap().generation;
    let later = 10 + BINDING_IDLE_TTL_SECS + 1;
    assert!(t.lookup("p", &s, later).is_none());
    assert_eq!(
        t.commit("p", &s, &acct("b"), 0, later),
        CommitOutcome::Placed
    );
    assert_eq!(
        t.commit("p", &s, &acct("c"), old, later + 1),
        CommitOutcome::Stale
    );
    assert_eq!(t.lookup("p", &s, later + 2).unwrap().account, acct("b"));

    // Removal by LRU eviction.
    let mut t = SessionBindings::with_capacity(2);
    t.commit("p", &s, &acct("a"), 0, 10);
    let old = t.lookup("p", &s, 10).unwrap().generation;
    t.commit("p", &h("x"), &acct("a"), 0, 20);
    t.commit("p", &h("y"), &acct("a"), 0, 21);
    assert!(t.lookup("p", &s, 22).is_none(), "evicted");
    assert_eq!(t.commit("p", &s, &acct("b"), 0, 23), CommitOutcome::Placed);
    assert_eq!(t.commit("p", &s, &acct("c"), old, 24), CommitOutcome::Stale);
    assert_eq!(t.lookup("p", &s, 25).unwrap().account, acct("b"));
}
