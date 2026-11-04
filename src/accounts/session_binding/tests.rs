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

#[test]
fn weight_is_deterministic_and_discriminating() {
    let s = h("s");
    let a = acct("a");
    let digest = {
        let d = Sha256::digest(serde_json::to_vec(&a).unwrap());
        u64::from_be_bytes(d[..8].try_into().unwrap())
    };
    assert_eq!(account_digest(&a), digest);
    let expected = {
        let mut z = u64::from_be_bytes(s[..8].try_into().unwrap()) ^ digest;
        z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    assert_eq!(weight(&s, digest), expected);
    assert_ne!(weight(&s, digest), weight(&s, account_digest(&acct("b"))));
    assert_ne!(weight(&s, digest), weight(&h("t"), digest));
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
