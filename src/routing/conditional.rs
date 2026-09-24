//! Runtime evaluation for `[models.router] type = "conditional"`.
//!
//! Pure lane: synchronous inside `resolve_chain`, no model calls, no drive. The
//! per-request cost is one rule walk (the rules are kept in `priority` order)
//! and one clock read.
//!
//! The config surface lives in `crate::config::router::conditional`; this module
//! is the layer allowed to read request state (headers) and the clock, per the
//! dependency table in `ARCHITECTURE.md` (`config` depends on nothing).

use time::OffsetDateTime;

use crate::config::router::conditional::{ConditionalRouterConfig, ConditionalRule, Weekday};
use crate::routing::outcome::RouteSource;

/// Everything a conditional rule's conditions can read, gathered once per
/// request so the rule walk never re-reads the clock or re-scans headers.
pub(crate) struct ConditionalContext<'a> {
    /// The request's model id, already past `strip_context_window_hint`.
    pub model: &'a str,
    pub now_hour: u8,
    pub now_minute: u8,
    pub now_weekday: Weekday,
    /// The inbound headers, or `None` for a body-less resolution (discovery,
    /// `count_tokens`, `shunt check`). A header condition never matches a
    /// `None` context, which is the fail-open behavior a metadata-gated rule
    /// should have on a surface that carries no request.
    headers: Option<&'a axum::http::HeaderMap>,
}

impl<'a> ConditionalContext<'a> {
    /// Gather the context for a live request (`headers` present) or a body-less
    /// one (`headers` absent), reading the clock at `now` shifted by
    /// `utc_offset_hours`.
    ///
    /// `now` is a parameter rather than a `now_utc()` call inside so the rule
    /// walk is deterministic under test — a time-window assertion that read the
    /// real clock would pass or fail by the wall time it happened to run at.
    fn from_time(
        model: &'a str,
        headers: Option<&'a axum::http::HeaderMap>,
        utc_offset_hours: f64,
        now: OffsetDateTime,
    ) -> Self {
        let shifted = now.to_offset(utc_offset(utc_offset_hours));
        let (now_hour, now_minute, now_weekday) = decompose_time(&shifted);
        Self {
            model,
            now_hour,
            now_minute,
            now_weekday,
            headers,
        }
    }

    /// The value of `name`, matched case-insensitively. `None` when the request
    /// carries no such header or there is no request at all.
    fn header_value(&self, name: &str) -> Option<&'a str> {
        let headers = self.headers?;
        headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// The `UtcOffset` for `offset_hours`.
///
/// The offset is built from *whole seconds* so a negative fractional hour is
/// exact: `-0.5` must shift the clock back thirty minutes, and decomposing it
/// into an `(hour, minute)` pair independently would encode `-00:30` as
/// `+00:30` (truncation toward zero loses the sign of the minute component).
/// An out-of-range offset falls back to UTC rather than panicking; config
/// validation rejects one.
fn utc_offset(offset_hours: f64) -> time::UtcOffset {
    let seconds = (offset_hours * 3600.0).round() as i32;
    time::UtcOffset::from_whole_seconds(seconds).unwrap_or(time::UtcOffset::UTC)
}

fn decompose_time(now: &OffsetDateTime) -> (u8, u8, Weekday) {
    let weekday = match now.weekday() {
        time::Weekday::Monday => Weekday::Mon,
        time::Weekday::Tuesday => Weekday::Tue,
        time::Weekday::Wednesday => Weekday::Wed,
        time::Weekday::Thursday => Weekday::Thu,
        time::Weekday::Friday => Weekday::Fri,
        time::Weekday::Saturday => Weekday::Sat,
        time::Weekday::Sunday => Weekday::Sun,
    };
    (now.hour(), now.minute(), weekday)
}

/// Whether one rule's `when` table holds for this request.
///
/// Every present condition must match. A header condition requires a live
/// request; a body-less context never satisfies one.
fn rule_matches(rule: &ConditionalRule, ctx: &ConditionalContext<'_>) -> bool {
    let when = &rule.when;
    if let Some(time) = &when.time_between {
        if !time.contains(ctx.now_hour, ctx.now_minute) {
            return false;
        }
    }
    if let Some(header) = &when.header {
        match ctx.header_value(&header.name) {
            Some(value) => {
                if !header.matches(value) {
                    return false;
                }
            }
            None => return false,
        }
    }
    if let Some(prefix) = &when.model_starts_with {
        if !ctx.model.starts_with(prefix.as_str()) {
            return false;
        }
    }
    if let Some(days) = &when.days {
        if !days.contains(&ctx.now_weekday) {
            return false;
        }
    }
    true
}

/// Select the target from a conditional router.
///
/// Rules are walked in `priority` order (lower first), tie-broken by declared
/// order, and the first match wins. Absent a match, `default_target` answers.
/// The returned target is a public model id, so the caller re-enters
/// `resolve_chain` for it and inherits failover, pools, and adapters.
pub(crate) fn select<'a>(
    config: &'a ConditionalRouterConfig,
    model: &str,
    headers: Option<&axum::http::HeaderMap>,
) -> (&'a str, RouteSource) {
    select_at(config, model, headers, OffsetDateTime::now_utc())
}

/// [`select`] against an explicit instant, so the clock is a test input rather
/// than ambient state.
fn select_at<'a>(
    config: &'a ConditionalRouterConfig,
    model: &str,
    headers: Option<&axum::http::HeaderMap>,
    now: OffsetDateTime,
) -> (&'a str, RouteSource) {
    let ctx =
        ConditionalContext::from_time(model, headers, config.utc_offset_hours.unwrap_or(0.0), now);

    // `rules` is held in `priority` order by the config load's sort, so this
    // walk needs no per-request sort.
    for rule in &config.rules {
        if rule_matches(rule, &ctx) {
            return (rule.target.as_str(), RouteSource::Conditional);
        }
    }

    (
        config.default_target.as_str(),
        RouteSource::ConditionalDefault,
    )
}

#[cfg(test)]
mod tests;
