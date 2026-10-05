use std::sync::Arc;

use super::*;
use crate::config::PricingConfig;

fn limit(scope: Scope, period: Period, amount: Option<&str>) -> SpendLimit {
    SpendLimit {
        id: "spl_t".into(),
        amount: amount.map(str::to_string),
        created_at: String::new(),
        currency: "USD".into(),
        period,
        scope,
        object_type: "spend_limit".into(),
        updated_at: String::new(),
    }
}

fn user(id: &str) -> Scope {
    Scope::User { user_id: id.into() }
}

/// 2026-10-07 (a Wednesday) 12:34:56 UTC.
const WED: u64 = 1_791_376_496;
const DAY: u64 = 86_400;

#[test]
fn windows_roll_at_utc_boundaries() {
    // 2026-10-07 00:00 UTC is day 20733.
    let midnight = WED - WED % DAY;
    assert_eq!(window(Period::Daily, WED).start, midnight);
    assert_eq!(window(Period::Daily, WED).end, midnight + DAY);
    assert_eq!(window(Period::Daily, midnight - 1).end, midnight);
    // Monday 2026-10-05.
    let monday = midnight - 2 * DAY;
    assert_eq!(window(Period::Weekly, WED).start, monday);
    assert_eq!(window(Period::Weekly, WED).end, monday + 7 * DAY);
    assert_eq!(window(Period::Weekly, monday).start, monday);
    assert_eq!(window(Period::Weekly, monday - 1).end, monday);
    // Month: 2026-10-01 .. 2026-11-01.
    let oct1 = midnight - 6 * DAY;
    let nov1 = oct1 + 31 * DAY;
    assert_eq!(
        window(Period::Monthly, WED),
        Window {
            start: oct1,
            end: nov1
        }
    );
    assert_eq!(window(Period::Monthly, nov1 - 1).start, oct1);
    assert_eq!(window(Period::Monthly, nov1).start, nov1);
}

#[test]
fn monthly_window_handles_year_end_and_leap_february() {
    // 2026-12-31 23:59:59 -> resets 2027-01-01 00:00 (1_798_761_600).
    assert_eq!(window(Period::Monthly, 1_798_761_599).end, 1_798_761_600);
    // 2028-02-16 (leap year): resets 2028-03-01 00:00 UTC = 1_835_481_600.
    assert_eq!(window(Period::Monthly, 1_834_272_000).end, 1_835_481_600);
}

#[test]
fn cap_resolves_user_then_org_then_unlimited() {
    let limits = vec![
        limit(Scope::Organization, Period::Daily, Some("100")),
        limit(user("alice"), Period::Daily, Some("5")),
        limit(Scope::Organization, Period::Weekly, Some("700")),
        limit(user("bob"), Period::Weekly, None),
    ];
    let cents = |c: u128| Some(c * FEMTO_USD_PER_CENT);
    assert_eq!(effective_cap(&limits, "alice", Period::Daily), cents(5));
    assert_eq!(effective_cap(&limits, "carol", Period::Daily), cents(100));
    // A user row with no amount is an explicit unlimited that beats the org cap.
    assert_eq!(effective_cap(&limits, "bob", Period::Weekly), None);
    assert_eq!(effective_cap(&limits, "carol", Period::Weekly), cents(700));
    assert_eq!(effective_cap(&limits, "alice", Period::Monthly), None);
}

#[test]
fn org_cap_is_per_principal_not_shared() {
    let limits = vec![limit(Scope::Organization, Period::Daily, Some("1"))];
    let meter = SpendMeter::default();
    meter.record("a", WED, (FEMTO_USD_PER_CENT - 1) as u64);
    meter.record("b", WED, (FEMTO_USD_PER_CENT - 1) as u64);
    assert_eq!(meter.check(&limits, "a", WED), Check::Allow);
    assert_eq!(meter.check(&limits, "b", WED), Check::Allow);
}

#[test]
fn zero_cap_blocks_and_reached_means_greater_or_equal() {
    let meter = SpendMeter::default();
    let zero = vec![limit(Scope::Organization, Period::Daily, Some("0"))];
    let reset = window(Period::Daily, WED).end;
    assert_eq!(
        meter.check(&zero, "a", WED),
        Check::Blocked {
            period: Period::Daily,
            reset_at: reset
        }
    );
    let one = vec![limit(user("a"), Period::Daily, Some("1"))];
    meter.record("a", WED, (FEMTO_USD_PER_CENT - 1) as u64);
    assert_eq!(meter.check(&one, "a", WED), Check::Allow);
    meter.record("a", WED, 1);
    assert!(matches!(meter.check(&one, "a", WED), Check::Blocked { .. }));
    // Next UTC day the daily window is fresh.
    assert_eq!(meter.check(&one, "a", WED + DAY), Check::Allow);
}

#[test]
fn check_names_the_cap_that_resets_last() {
    let limits = vec![
        limit(user("a"), Period::Daily, Some("1")),
        limit(user("a"), Period::Weekly, Some("1")),
        limit(user("a"), Period::Monthly, Some("1")),
    ];
    let meter = SpendMeter::default();
    meter.record("a", WED, FEMTO_USD_PER_CENT as u64);
    assert_eq!(
        meter.check(&limits, "a", WED),
        Check::Blocked {
            period: Period::Monthly,
            reset_at: window(Period::Monthly, WED).end
        }
    );
    // Only daily and weekly reached: weekly resets last.
    let no_month = &limits[..2];
    assert_eq!(
        meter.check(no_month, "a", WED),
        Check::Blocked {
            period: Period::Weekly,
            reset_at: window(Period::Weekly, WED).end
        }
    );
}

#[test]
fn unavailable_principal_is_reported() {
    let meter = SpendMeter::default();
    meter.mark_unavailable("a");
    assert_eq!(meter.check(&[], "a", WED), Check::Unavailable);
    assert_eq!(meter.check(&[], "b", WED), Check::Allow);
    meter.clear_unavailable("a");
    assert_eq!(meter.check(&[], "a", WED), Check::Allow);
}

#[test]
fn record_saturates() {
    let meter = SpendMeter::default();
    meter.record("a", WED, u64::MAX);
    meter.record("a", WED, 10);
    assert_eq!(meter.spent("a", Period::Daily, WED), u64::MAX);
}

#[test]
fn concurrent_records_lose_nothing() {
    let meter = Arc::new(SpendMeter::default());
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let meter = Arc::clone(&meter);
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    meter.record("a", WED, 7);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    for period in PERIODS {
        assert_eq!(meter.spent("a", period, WED), 8 * 1_000 * 7);
    }
}

fn usage() -> RequestUsage {
    RequestUsage {
        tokens: Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
        },
        web_search_requests: 0,
    }
}

const FEMTO_PER_USD: u64 = 1_000_000_000_000_000;

#[test]
fn unknown_model_costs_the_fallback_rate() {
    let meter = SpendMeter::default();
    let table = PriceTable::from_config(None);
    // 5 + 25 + 0.50 + 6.25 = 36.75 USD for one million of each.
    let cost = meter.cost(&table, "up", "mystery-1", "mystery-1", &usage());
    assert_eq!(cost, 36 * FEMTO_PER_USD + FEMTO_PER_USD * 3 / 4);
}

#[test]
fn unknown_model_rate_is_scaled_by_the_multiplier() {
    let meter = SpendMeter::default();
    let table = PriceTable::from_config(Some(&PricingConfig {
        multiplier: 0.5,
        overrides: Vec::new(),
    }));
    let cost = meter.cost(&table, "up", "mystery-1", "mystery-1", &usage());
    assert_eq!(cost, (36 * FEMTO_PER_USD + FEMTO_PER_USD * 3 / 4) / 2);
}

#[test]
fn priced_model_uses_resolved_rates_and_web_search() {
    let meter = SpendMeter::default();
    let table = PriceTable::from_config(None);
    let model = crate::gateway::spend::pricing::LIST_PRICES[0].0;
    let rates = table.resolve("up", model, model).expect("catalog row");
    let mut u = usage();
    u.web_search_requests = 3;
    let expected = rates.cost_femto_usd(&u.tokens) + 3 * table.web_search_cost_femto_usd();
    assert_eq!(meter.cost(&table, "up", model, model, &u), expected);
}

#[test]
fn unknown_model_warns_once_per_id() {
    let meter = SpendMeter::default();
    let table = PriceTable::from_config(None);
    for _ in 0..3 {
        meter.cost(&table, "up", "m1", "m1", &usage());
    }
    meter.cost(&table, "up", "m2", "m2", &usage());
    assert_eq!(meter.warned_models.lock().unwrap().len(), 2);
    assert!(!meter.first_sighting("m1"));
    assert!(meter.first_sighting("m3"));
}

#[test]
fn warned_models_stop_growing_at_the_cap_and_pricing_is_unchanged() {
    let meter = SpendMeter::default();
    let table = PriceTable::from_config(None);
    for n in 0..MAX_WARNED_MODELS {
        assert!(meter.first_sighting(&format!("filler-{n}")));
    }
    let unknown_rate = meter.cost(&table, "up", "filler-0", "filler-0", &usage());
    // A known id at the cap is not a new one being dropped.
    assert!(!meter.first_sighting("filler-1"));
    assert!(!meter.warned_cap_reported.load(Ordering::Relaxed));
    assert!(!meter.first_sighting("one-too-many"));
    assert!(meter.warned_cap_reported.load(Ordering::Relaxed));
    assert!(!meter.first_sighting("two-too-many"));
    assert!(meter.warned_cap_reported.load(Ordering::Relaxed));
    assert_eq!(meter.warned_models.lock().unwrap().len(), MAX_WARNED_MODELS);
    assert_eq!(
        meter.cost(&table, "up", "past-cap", "past-cap", &usage()),
        unknown_rate
    );
    assert_eq!(unknown_rate, 36 * FEMTO_PER_USD + FEMTO_PER_USD * 3 / 4);
    assert_eq!(meter.warned_models.lock().unwrap().len(), MAX_WARNED_MODELS);
}

#[test]
fn reset_label_names_the_utc_midnight_of_the_given_day() {
    assert_eq!(reset_label(0), "1970-01-01 00:00 UTC");
    // 2026-10-04 12:34:56 UTC, mid-day: the label is still that date's midnight.
    assert_eq!(reset_label(1_791_117_296), "2026-10-04 00:00 UTC");
    assert_eq!(
        reset_label(window(Period::Monthly, 1_791_117_296).end),
        "2026-11-01 00:00 UTC"
    );
}
