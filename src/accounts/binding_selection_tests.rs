//! Selection-level tests for session bindings and assignment strategies
//! (ADR-0008 §2–§3; spec AC-002..AC-013, AC-018, AC-020, FR-15).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};

use super::*;
use crate::config::PoolAssignment;

const P: &str = "anthropic";

fn account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        ..Default::default()
    }
}

fn pool_accounts(n: usize) -> Vec<AccountConfig> {
    (0..n).map(|i| account(&format!("acct-{i}"))).collect()
}

fn soft(threshold: f64) -> PoolConfig {
    PoolConfig {
        default_threshold: Some(threshold),
        ..Default::default()
    }
}

fn note(pool: &AccountPool, account: &AccountConfig, headers: &[(&'static str, &str)]) {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(*name, HeaderValue::from_str(value).unwrap());
    }
    pool.note_quota(P, account, &map);
}

fn note_5h(pool: &AccountPool, account: &AccountConfig, utilization: &str) {
    note(
        pool,
        account,
        &[("anthropic-ratelimit-unified-5h-utilization", utilization)],
    );
}

/// Select for `session` and commit the head as the serving account.
fn serve(
    pool: &AccountPool,
    accounts: &[AccountConfig],
    session: &str,
    cfg: Option<&PoolConfig>,
) -> (usize, BindingCommit) {
    let (order, ticket) = pool.select_order_with_ticket(P, accounts, Some(session), None, cfg);
    let head = order[0];
    let commit = pool.commit_session_binding(&ticket.unwrap(), &accounts[head], false);
    (head, commit)
}

fn head(
    pool: &AccountPool,
    accounts: &[AccountConfig],
    session: &str,
    cfg: Option<&PoolConfig>,
) -> usize {
    pool.select_order(P, accounts, Some(session), None, cfg)[0]
}

fn bindings_len(pool: &AccountPool) -> usize {
    pool.bindings.lock().unwrap().len()
}

fn backdate_soft_near(pool: &AccountPool, account: &AccountConfig, by: Duration) {
    let mut entries = pool.entries.lock().unwrap();
    let health = entries.get_mut(&account_key(P, account)).unwrap();
    let since = health.soft_near_since.expect("soft-near was stamped");
    health.soft_near_since = Some(since.checked_sub(by).unwrap());
}

#[test]
fn ticketless_selection_never_inserts_and_placement_is_stable() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(4);
    let first = head(&pool, &accounts, "s", None);
    for _ in 0..8 {
        assert_eq!(head(&pool, &accounts, "s", None), first);
    }
    assert_eq!(bindings_len(&pool), 0);
}

#[test]
fn commit_places_then_touches() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (first, commit) = serve(&pool, &accounts, "s", None);
    assert_eq!(commit, BindingCommit::Placed);
    let (again, commit) = serve(&pool, &accounts, "s", None);
    assert_eq!(again, first);
    assert_eq!(commit, BindingCommit::Touched);
    assert_eq!(bindings_len(&pool), 1);
}

#[test]
fn sessionless_and_empty_ids_round_robin_without_a_ticket() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    for session in [None, Some("")] {
        let starts = (0..3)
            .map(|_| {
                let (order, ticket) =
                    pool.select_order_with_ticket(P, &accounts, session, None, None);
                assert!(ticket.is_none(), "FR-15: no binding for {session:?}");
                order[0]
            })
            .collect::<Vec<_>>();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2],
            "round robin over {session:?}: {starts:?}"
        );
    }
    assert_eq!(bindings_len(&pool), 0);
}

#[test]
fn soft_near_bound_account_is_debounced_and_reset_by_a_non_near_assessment() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let cfg = soft(0.5);
    let (bound, _) = serve(&pool, &accounts, "s", Some(&cfg));

    // AC-006: soft-near for under 60 s keeps the session.
    note_5h(&pool, &accounts[bound], "0.6");
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), bound);
    backdate_soft_near(&pool, &accounts[bound], Duration::from_secs(59));
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), bound);

    // AC-007: a non-near assessment restarts the count from zero.
    note_5h(&pool, &accounts[bound], "0.1");
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), bound);
    note_5h(&pool, &accounts[bound], "0.6");
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), bound);
    backdate_soft_near(&pool, &accounts[bound], Duration::from_secs(59));
    assert_eq!(
        head(&pool, &accounts, "s", Some(&cfg)),
        bound,
        "59 s since the reset, not 118 s in total"
    );

    // Past the debounce the session moves, reported as proactive.
    backdate_soft_near(&pool, &accounts[bound], Duration::from_secs(1));
    let (order, ticket) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, Some(&cfg));
    assert_ne!(order[0], bound);
    let ticket = ticket.unwrap();
    assert_eq!(ticket.prior_ineligible(), Some(SwitchReason::Proactive));
    assert_eq!(
        pool.commit_session_binding(&ticket, &accounts[order[0]], false),
        BindingCommit::Moved(SwitchReason::Proactive)
    );
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), order[0]);
}

#[test]
fn soft_near_tracking_is_separate_for_fable_requests() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(2);
    let cfg = soft(0.5);
    note_5h(&pool, &accounts[0], "0.6");
    pool.select_order(P, &accounts, Some("s"), None, Some(&cfg));
    let entries = pool.entries.lock().unwrap();
    let health = &entries[&account_key(P, &accounts[0])];
    assert!(health.soft_near_since.is_some());
    assert!(health.soft_near_since_fable.is_none());
}

#[test]
fn immediate_ineligibility_moves_on_the_same_request() {
    // AC-008: hard-over, rejected, paused, disabled, capped and cooling.
    type Break = fn(&AccountPool, &mut Vec<AccountConfig>, usize);
    let cases: [(&str, Break, SwitchReason); 6] = [
        (
            "hard-over",
            |pool, accounts, i| note_5h(pool, &accounts[i], "0.99"),
            SwitchReason::Proactive,
        ),
        (
            "rejected",
            |pool, accounts, i| {
                note(
                    pool,
                    &accounts[i],
                    &[("anthropic-ratelimit-unified-5h-status", "rejected")],
                )
            },
            SwitchReason::Proactive,
        ),
        (
            "paused",
            |pool, accounts, i| pool.set_paused(P, &accounts[i], true),
            SwitchReason::Proactive,
        ),
        (
            "disabled",
            |_, accounts, i| accounts[i].disabled = true,
            SwitchReason::Proactive,
        ),
        (
            "capped",
            |pool, accounts, i| {
                accounts[i].max_utilization = Some(0.4);
                note_5h(pool, &accounts[i], "0.45");
            },
            SwitchReason::Proactive,
        ),
        (
            "cooling",
            |pool, accounts, i| pool.cooldown(P, &accounts[i], Duration::from_secs(300), "test"),
            SwitchReason::Cooldown,
        ),
    ];
    for (label, breaks, reason) in cases {
        let pool = AccountPool::new();
        let mut accounts = pool_accounts(3);
        let cfg = soft(0.5);
        let (bound, _) = serve(&pool, &accounts, "s", Some(&cfg));
        breaks(&pool, &mut accounts, bound);
        let (order, ticket) =
            pool.select_order_with_ticket(P, &accounts, Some("s"), None, Some(&cfg));
        assert_ne!(order[0], bound, "{label}: moves without waiting");
        let ticket = ticket.unwrap();
        assert_eq!(ticket.prior_ineligible(), Some(reason), "{label}");
        assert_eq!(
            pool.commit_session_binding(&ticket, &accounts[order[0]], false),
            BindingCommit::Moved(reason),
            "{label}"
        );
    }
}

#[test]
fn moved_session_stays_after_the_old_account_recovers() {
    // AC-002 / FR-4.
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (a, _) = serve(&pool, &accounts, "s", None);
    pool.cooldown(P, &accounts[a], Duration::from_secs(300), "test");
    let (b, commit) = serve(&pool, &accounts, "s", None);
    assert_ne!(a, b);
    assert_eq!(commit, BindingCommit::Moved(SwitchReason::Cooldown));
    pool.mark_healthy(P, &accounts[a], true);
    assert_eq!(head(&pool, &accounts, "s", None), b, "no pull back to {a}");
    let (served, commit) = serve(&pool, &accounts, "s", None);
    assert_eq!(served, b);
    assert_eq!(commit, BindingCommit::Touched);
}

#[test]
fn fable_scoped_limit_moves_only_fable_requests() {
    // AC-005: a Fable-only cooldown displaces Fable traffic, not other models.
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (a, _) = serve(&pool, &accounts, "s", None);
    pool.cooldown_scoped(
        P,
        &accounts[a],
        Duration::from_secs(300),
        "test",
        CooldownScope::Fable,
    );
    let order = pool.select_order(P, &accounts, Some("s"), Some("claude-sonnet-4"), None);
    assert_eq!(order[0], a);
    let (order, ticket) =
        pool.select_order_with_ticket(P, &accounts, Some("s"), Some("claude-fable-5"), None);
    assert_ne!(order[0], a);
    assert_eq!(
        ticket.unwrap().prior_ineligible(),
        Some(SwitchReason::Cooldown)
    );
}

#[test]
fn bound_account_attempted_and_failed_moves_as_failover() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (a, _) = serve(&pool, &accounts, "s", None);
    let (order, ticket) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, None);
    assert_eq!(order[0], a);
    let ticket = ticket.unwrap();
    assert!(ticket.is_bound());
    assert_eq!(ticket.prior_ineligible(), None);
    // The adapter tried `a`, it failed over, and order[1] served.
    assert_eq!(
        pool.commit_session_binding(&ticket, &accounts[order[1]], true),
        BindingCommit::Moved(SwitchReason::Failover)
    );
    assert_eq!(head(&pool, &accounts, "s", None), order[1]);
}

#[test]
fn another_account_serving_for_no_reason_leaves_the_binding() {
    // Admission skip: the bound account was healthy and not attempted.
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (a, _) = serve(&pool, &accounts, "s", None);
    let (order, ticket) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, None);
    assert_eq!(
        pool.commit_session_binding(&ticket.unwrap(), &accounts[order[1]], false),
        BindingCommit::Unchanged
    );
    assert_eq!(head(&pool, &accounts, "s", None), a);
}

#[test]
fn reprobe_promotion_serving_leaves_the_binding() {
    let cfg = PoolConfig {
        default_threshold: Some(0.5),
        reprobe_seconds: Some(60),
        ..Default::default()
    };
    let pool = Arc::new(AccountPool::new());
    let accounts = ["codex-a", "codex-b", "codex-c"]
        .map(|name| AccountConfig {
            store_family: Some(StoreFamily::Chatgpt),
            ..account(name)
        })
        .to_vec();
    let (order, ticket) =
        pool.select_order_with_ticket("codex", &accounts, Some("s"), None, Some(&cfg));
    let bound = order[0];
    pool.commit_session_binding(&ticket.unwrap(), &accounts[bound], false);
    let stale = (bound + 1) % accounts.len();
    {
        let mut entries = pool.entries.lock().unwrap();
        let health = entries
            .entry(account_key("codex", &accounts[stale]))
            .or_default();
        health.quota.utilization_5h = Some(0.9);
        health.quota.observed_at_5h = Some(unix_now() - 61);
    }
    let (order, reservation, _, ticket) = pool.select_order_deferred_with_cap_and_ticket(
        "codex",
        &accounts,
        Some("s"),
        None,
        Some(&cfg),
    );
    assert_eq!(
        order[0], stale,
        "the re-probe is promoted ahead of the binding"
    );
    assert!(reservation.is_some());
    let ticket = ticket.unwrap();
    assert_eq!(
        pool.commit_session_binding(&ticket, &accounts[stale], false),
        BindingCommit::Unchanged
    );
    drop(reservation);
    let still_bound = pool
        .bindings
        .lock()
        .unwrap()
        .lookup("codex", &ticket.session, unix_now())
        .unwrap();
    assert_eq!(still_bound.account, account_key("codex", &accounts[bound]));
}

#[test]
fn an_older_ticket_cannot_overwrite_a_newer_binding() {
    // NFR-1 through the pool API.
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (a, _) = serve(&pool, &accounts, "s", None);
    let (_, slow) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, None);
    let (order, fast) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, None);
    let b = order[1];
    assert_eq!(
        pool.commit_session_binding(&fast.unwrap(), &accounts[b], true),
        BindingCommit::Moved(SwitchReason::Failover)
    );
    let c = (0..3).find(|&i| i != a && i != b).unwrap();
    assert_eq!(
        pool.commit_session_binding(&slow.unwrap(), &accounts[c], true),
        BindingCommit::Stale
    );
    assert_eq!(head(&pool, &accounts, "s", None), b);

    // Two first requests race: the second placement loses.
    let (_, one) = pool.select_order_with_ticket(P, &accounts, Some("t"), None, None);
    let (_, two) = pool.select_order_with_ticket(P, &accounts, Some("t"), None, None);
    assert_eq!(
        pool.commit_session_binding(&one.unwrap(), &accounts[0], false),
        BindingCommit::Placed
    );
    assert_eq!(
        pool.commit_session_binding(&two.unwrap(), &accounts[1], false),
        BindingCommit::Stale
    );
}

#[test]
fn spread_places_about_one_in_n_plus_one_on_an_added_account() {
    // AC-009, and rendezvous keeps every other placement where it was.
    let pool = AccountPool::new();
    let mut accounts = pool_accounts(4);
    let sessions = (0..2000)
        .map(|i| format!("session-{i}"))
        .collect::<Vec<_>>();
    let before = sessions
        .iter()
        .map(|s| head(&pool, &accounts, s, None))
        .collect::<Vec<_>>();
    accounts.push(account("added"));
    let mut on_added = 0;
    for (session, &old) in sessions.iter().zip(&before) {
        let now = head(&pool, &accounts, session, None);
        if now == 4 {
            on_added += 1;
        } else {
            assert_eq!(now, old, "only sessions won by the added account move");
        }
    }
    let share = f64::from(on_added) / sessions.len() as f64;
    assert!(
        (0.15..0.25).contains(&share),
        "share on added account: {share}"
    );
}

#[test]
fn adding_an_account_leaves_existing_bindings() {
    // AC-010.
    let pool = AccountPool::new();
    let mut accounts = pool_accounts(3);
    let bound = (0..200)
        .map(|i| {
            let session = format!("s-{i}");
            let (head, _) = serve(&pool, &accounts, &session, None);
            (session, head)
        })
        .collect::<Vec<_>>();
    accounts.push(account("added"));
    for (session, head_before) in bound {
        assert_eq!(head(&pool, &accounts, &session, None), head_before);
    }
}

#[test]
fn displaced_sessions_spread_by_their_own_weights() {
    // AC-011.
    let pool = AccountPool::new();
    let accounts = pool_accounts(5);
    let on_zero = (0..400)
        .map(|i| format!("s-{i}"))
        .filter(|s| head(&pool, &accounts, s, None) == 0)
        .collect::<Vec<_>>();
    for session in &on_zero {
        serve(&pool, &accounts, session, None);
    }
    pool.cooldown(P, &accounts[0], Duration::from_secs(300), "test");
    let mut landed = HashMap::<usize, usize>::new();
    for session in &on_zero {
        let (served, commit) = serve(&pool, &accounts, session, None);
        assert_eq!(commit, BindingCommit::Moved(SwitchReason::Cooldown));
        *landed.entry(served).or_default() += 1;
    }
    assert!(!landed.contains_key(&0));
    assert_eq!(
        landed.len(),
        4,
        "displaced sessions use every survivor: {landed:?}"
    );
}

#[test]
fn placement_is_restart_stable_without_persistence() {
    // AC-012: two process lifetimes, same pool and health.
    let accounts = pool_accounts(6);
    let first = AccountPool::new();
    let second = AccountPool::new();
    for i in 0..100 {
        let session = format!("s-{i}");
        assert_eq!(
            head(&first, &accounts, &session, None),
            head(&second, &accounts, &session, None)
        );
    }
}

#[test]
fn soonest_weekly_reset_places_and_moves_but_never_pulls_back() {
    // AC-013 / FR-8.
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let cfg = PoolConfig {
        assignment: PoolAssignment::SoonestWeeklyReset,
        ..Default::default()
    };
    let now = unix_now();
    for (index, reset_in) in [(0usize, 9_000u64), (1, 3_000), (2, 6_000)] {
        note(
            &pool,
            &accounts[index],
            &[
                ("anthropic-ratelimit-unified-7d-utilization", "0.1"),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    &(now + reset_in).to_string(),
                ),
            ],
        );
    }
    let (placed, commit) = serve(&pool, &accounts, "s", Some(&cfg));
    assert_eq!((placed, commit), (1, BindingCommit::Placed));

    // Displaced: next soonest reset.
    pool.cooldown(P, &accounts[1], Duration::from_secs(300), "test");
    let (moved, commit) = serve(&pool, &accounts, "s", Some(&cfg));
    assert_eq!(
        (moved, commit),
        (2, BindingCommit::Moved(SwitchReason::Cooldown))
    );

    // The soonest account recovers: new sessions go there, the moved one stays.
    pool.mark_healthy(P, &accounts[1], true);
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), 2);
    assert_eq!(head(&pool, &accounts, "fresh", Some(&cfg)), 1);
}

#[test]
fn soonest_weekly_reset_respects_priority_then_unknown_last() {
    let pool = AccountPool::new();
    let mut accounts = pool_accounts(3);
    accounts[0].priority = 50;
    let cfg = PoolConfig {
        assignment: PoolAssignment::SoonestWeeklyReset,
        ..Default::default()
    };
    note(
        &pool,
        &accounts[2],
        &[
            ("anthropic-ratelimit-unified-7d-utilization", "0.1"),
            (
                "anthropic-ratelimit-unified-7d-reset",
                &(unix_now() + 3_000).to_string(),
            ),
        ],
    );
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), 0, "priority first");
    accounts[0].priority = 100;
    assert_eq!(
        head(&pool, &accounts, "s", Some(&cfg)),
        2,
        "known reset before unknown"
    );
}

#[test]
fn removed_account_rebinds_as_failover() {
    // AC-018.
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (a, _) = serve(&pool, &accounts, "s", None);
    let remaining = accounts
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != a)
        .map(|(_, account)| account.clone())
        .collect::<Vec<_>>();
    let (order, ticket) = pool.select_order_with_ticket(P, &remaining, Some("s"), None, None);
    let ticket = ticket.unwrap();
    assert!(ticket.is_bound());
    assert_eq!(ticket.prior_ineligible(), Some(SwitchReason::Failover));
    assert_eq!(
        pool.commit_session_binding(&ticket, &remaining[order[0]], false),
        BindingCommit::Moved(SwitchReason::Failover)
    );
    assert_eq!(head(&pool, &remaining, "s", None), order[0]);
}

#[test]
fn sort_by_reset_override_leaves_bound_heads_in_place() {
    // AC-020 / FR-16.
    let pool = AccountPool::new();
    let accounts = pool_accounts(4);
    let cfg = PoolConfig::default();
    let now = unix_now();
    let bound = (0..40)
        .map(|i| {
            let session = format!("s-{i}");
            let (head, _) = serve(&pool, &accounts, &session, Some(&cfg));
            (session, head)
        })
        .collect::<Vec<_>>();
    for (index, account) in accounts.iter().enumerate() {
        note(
            &pool,
            account,
            &[
                ("anthropic-ratelimit-unified-5h-utilization", "0.1"),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    &(now + 1_000 * (index as u64 + 1)).to_string(),
                ),
            ],
        );
    }
    pool.set_sort_by_reset_override(Some(true));
    for (session, head_before) in bound {
        assert_eq!(head(&pool, &accounts, &session, Some(&cfg)), head_before);
    }
}

#[test]
fn a_moved_session_keeps_the_failover_order_after_its_head() {
    // FR-16: the tail after the head is today's sorted failover order.
    let pool = AccountPool::new();
    let accounts = pool_accounts(4);
    let cfg = PoolConfig {
        sort_by_reset: true,
        ..Default::default()
    };
    let (a, _) = serve(&pool, &accounts, "s", Some(&cfg));
    pool.cooldown(P, &accounts[a], Duration::from_secs(300), "test");
    let now = unix_now();
    let others = (0..4).filter(|&i| i != a).collect::<Vec<_>>();
    for (rank, &index) in others.iter().enumerate() {
        note(
            &pool,
            &accounts[index],
            &[
                ("anthropic-ratelimit-unified-5h-utilization", "0.1"),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    &(now + 1_000 * (3 - rank as u64)).to_string(),
                ),
            ],
        );
    }
    let order = pool.select_order(P, &accounts, Some("s"), None, Some(&cfg));
    let moved = order[0];
    let mut by_reset = others.iter().rev().copied().collect::<Vec<_>>();
    by_reset.retain(|&i| i != moved);
    by_reset.push(a);
    assert_eq!(
        order[1..],
        by_reset[..],
        "tail is soonest-reset first, cooled last"
    );
}

#[test]
fn forget_identity_drops_its_bindings() {
    let pool = AccountPool::new();
    let accounts = (0..3)
        .map(|i| AccountConfig {
            uuid: Some(format!("uuid-{i}")),
            ..account(&format!("acct-{i}"))
        })
        .collect::<Vec<_>>();
    for i in 0..60 {
        serve(&pool, &accounts, &format!("s-{i}"), None);
    }
    let total = bindings_len(&pool);
    let on_zero = (0..60)
        .filter(|i| head(&pool, &accounts, &format!("s-{i}"), None) == 0)
        .count();
    assert!(on_zero > 0);
    pool.forget_identity(StoreFamily::Claude, "uuid-0", None);
    assert_eq!(bindings_len(&pool), total - on_zero);
}

#[test]
fn soft_near_since_is_cleared_on_any_non_near_selection() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(2);
    let cfg = soft(0.5);
    note_5h(&pool, &accounts[0], "0.6");
    pool.select_order(P, &accounts, None, None, Some(&cfg));
    let stamped = {
        let entries = pool.entries.lock().unwrap();
        entries[&account_key(P, &accounts[0])].soft_near_since
    };
    assert!(stamped.is_some_and(|at| at <= Instant::now()));
    // Hard-over is near but not soft-near: the stamp clears.
    note_5h(&pool, &accounts[0], "0.99");
    pool.select_order(P, &accounts, None, None, Some(&cfg));
    let entries = pool.entries.lock().unwrap();
    assert!(entries[&account_key(P, &accounts[0])]
        .soft_near_since
        .is_none());
}

#[test]
fn ticket_names_the_bound_account_only() {
    let pool = AccountPool::new();
    let accounts = pool_accounts(3);
    let (_, ticket) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, None);
    let ticket = ticket.unwrap();
    assert!(accounts.iter().all(|account| !ticket.is_bound_to(account)));
    let (a, _) = serve(&pool, &accounts, "s", None);
    let (_, ticket) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, None);
    let ticket = ticket.unwrap();
    for (index, account) in accounts.iter().enumerate() {
        assert_eq!(ticket.is_bound_to(account), index == a, "account {index}");
    }
}

#[test]
fn a_placement_that_evicts_counts_the_evictions() {
    // AC-015 through the pool: the table is full, so the next placement
    // evicts, and the eviction counter receives exactly what the table evicted.
    let provider = "binding-eviction-metric";
    let pool = AccountPool::new();
    let accounts = pool_accounts(2);
    let place = |session: &str| {
        let (order, ticket) =
            pool.select_order_with_ticket(provider, &accounts, Some(session), None, None);
        pool.commit_session_binding(&ticket.unwrap(), &accounts[order[0]], false)
    };
    for i in 0..session_binding::MAX_BINDINGS {
        assert_eq!(place(&format!("s-{i}")), BindingCommit::Placed);
    }
    assert_eq!(
        crate::metrics::session_binding_eviction_count_for_tests(provider),
        0
    );
    assert_eq!(place("one-more"), BindingCommit::Placed);
    let evicted = pool.bindings.lock().unwrap().evictions();
    assert!(evicted > 0, "a full table evicts on the next placement");
    assert_eq!(
        crate::metrics::session_binding_eviction_count_for_tests(provider),
        evicted
    );
}

#[test]
fn soft_near_threshold_of_a_non_representative_alias_cannot_reset_the_debounce() {
    let pool = AccountPool::new();
    let mut accounts = pool_accounts(3);
    accounts[0].uuid = Some("shared".to_string());
    accounts[0].threshold = Some(0.5);
    accounts[1].uuid = Some("shared".to_string());
    accounts[1].threshold = Some(0.9);
    let cfg = soft(0.5);
    let session = (0..64)
        .map(|i| format!("alias-{i}"))
        .find(|s| serve(&pool, &accounts, s, Some(&cfg)).0 == 0)
        .expect("some session binds to the shared identity");
    note_5h(&pool, &accounts[0], "0.6");
    // Stamps soft-near through the representative only.
    pool.select_order(P, &accounts, Some(&session), None, Some(&cfg));
    backdate_soft_near(&pool, &accounts[0], Duration::from_secs(61));
    let (order, ticket) =
        pool.select_order_with_ticket(P, &accounts, Some(&session), None, Some(&cfg));
    assert_eq!(order[0], 2, "moves to the healthy alternative");
    assert_eq!(
        ticket.unwrap().prior_ineligible(),
        Some(SwitchReason::Proactive)
    );
}

#[test]
fn paused_bound_account_moves_even_when_every_alternative_is_soft_near() {
    // AC-008 / FR-3: only a soft-near move needs a healthy target.
    let pool = AccountPool::new();
    let accounts = pool_accounts(2);
    let cfg = soft(0.5);
    let (bound, _) = serve(&pool, &accounts, "s", Some(&cfg));
    let other = 1 - bound;
    note_5h(&pool, &accounts[other], "0.6");
    pool.set_paused(P, &accounts[bound], true);
    let (order, ticket) = pool.select_order_with_ticket(P, &accounts, Some("s"), None, Some(&cfg));
    assert_eq!(order[0], other, "the soft-near account serves");
    let ticket = ticket.unwrap();
    assert_eq!(ticket.prior_ineligible(), Some(SwitchReason::Proactive));
    assert_eq!(
        pool.commit_session_binding(&ticket, &accounts[other], false),
        BindingCommit::Moved(SwitchReason::Proactive)
    );
    pool.set_paused(P, &accounts[bound], false);
    assert_eq!(head(&pool, &accounts, "s", Some(&cfg)), other);
}
