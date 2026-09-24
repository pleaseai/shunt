//! `[models.router] type = "conditional"` — condition-based routing.
//!
//! A `[[models]]` entry whose `[models.router]` table is `type = "conditional"`
//! evaluates an ordered set of rules, each carrying composable match conditions
//! and a target public model id. The first rule whose conditions all match wins;
//! absent a match, the router's `default_target` answers. Conditions compose
//! with AND: a rule's `when` table may carry any subset of the supported keys,
//! and every present key must hold for the rule to fire.
//!
//! This module owns the **config surface only**: the wire types, their
//! round-trip, and the pure per-value predicates ([`TimeBetween::contains`],
//! [`HeaderMatch::matches`]) that do not read a request. The request-aware rule
//! walk lives in `crate::routing::conditional`, which is the layer allowed to
//! read headers and the clock (`config` depends on nothing, per the dependency
//! table in `ARCHITECTURE.md`).

use serde::{Deserialize, Serialize};

/// `[models.router]` with `type = "conditional"`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalRouterConfig {
    /// UTC offset in hours for time-window evaluation. Default: 0 (UTC).
    /// Fractional values are accepted (e.g. `5.5` for India, `8` for China).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_offset_hours: Option<f64>,
    /// The rules, evaluated in `priority` order (lower first).
    pub rules: Vec<ConditionalRule>,
    /// The public model id used when no rule matches. A body-less resolution
    /// (discovery, `count_tokens`) reads this too, so it must name a routable
    /// id — validation holds it to the same one-hop rule as a rule target.
    pub default_target: String,
}

/// One conditional-routing rule.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalRule {
    /// A human-readable label, surfaced as the rule's metric/log label.
    pub name: String,
    /// Evaluation order: lower numbers are checked first. Rules with equal
    /// priority keep their declared order.
    #[serde(default)]
    pub priority: u32,
    /// The public model id to route to when this rule matches.
    pub target: String,
    /// The conditions this rule requires. Every present condition must hold.
    pub when: WhenCondition,
}

/// Composable match conditions for a routing rule. Every present field must
/// match (AND); absent fields are ignored, so an empty table is a catch-all.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct WhenCondition {
    /// The clock (at the router's UTC offset) falls inside this window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_between: Option<TimeBetween>,
    /// A request header matches. A request with no such header does not match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<HeaderMatch>,
    /// The request's model id starts with this prefix (after `[1m]` stripping).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_starts_with: Option<String>,
    /// The current weekday is one of these. Absent means every day.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<Weekday>>,
}

impl WhenCondition {
    /// Whether this rule declares no condition at all — a catch-all. Used by
    /// validation to reject a catch-all that would shadow every later rule.
    pub fn is_catch_all(&self) -> bool {
        self.time_between.is_none()
            && self.header.is_none()
            && self.model_starts_with.is_none()
            && self.days.is_none()
    }
}

/// Parse `"HH:MM"` into `(hour, minute)`. `None` on malformed input, which
/// config validation turns into a load failure.
pub fn parse_hh_mm(s: &str) -> Option<(u8, u8)> {
    let (h, m) = s.split_once(':')?;
    // Reject a second separator so `1:2:3` is malformed rather than silently
    // parsed as `1:2`.
    if m.contains(':') {
        return None;
    }
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

fn ser_hh_mm<S: serde::Serializer>(v: &(u8, u8), s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{:02}:{:02}", v.0, v.1))
}

fn de_hh_mm<'de, D: serde::Deserializer<'de>>(d: D) -> Result<(u8, u8), D::Error> {
    let s = String::deserialize(d)?;
    parse_hh_mm(&s).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "invalid HH:MM time: expected 00:00-23:59, got {s:?}"
        ))
    })
}

/// A time-of-day window. Supports midnight crossing: `"22:00"` to `"08:00"`
/// matches 22:00:00 through 07:59:59 inclusive. Start is inclusive, end is
/// exclusive, in both the same-day and crossing forms.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeBetween {
    #[serde(deserialize_with = "de_hh_mm", serialize_with = "ser_hh_mm")]
    pub start: (u8, u8),
    #[serde(deserialize_with = "de_hh_mm", serialize_with = "ser_hh_mm")]
    pub end: (u8, u8),
}

impl TimeBetween {
    /// Whether `(hour, minute)` falls inside this window.
    pub fn contains(&self, hour: u8, minute: u8) -> bool {
        let now_min = hour as u16 * 60 + minute as u16;
        let start_min = self.start.0 as u16 * 60 + self.start.1 as u16;
        let end_min = self.end.0 as u16 * 60 + self.end.1 as u16;
        if start_min <= end_min {
            // Same-day window where start < end, e.g. 08:00 to 22:00.
            now_min >= start_min && now_min < end_min
        } else {
            // Crossing midnight, e.g. 22:00 to 08:00.
            now_min >= start_min || now_min < end_min
        }
    }
}

/// How a header value is compared against the condition's expected value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderMatchMode {
    /// Case-insensitive exact equality (default).
    #[default]
    Equals,
    /// Case-insensitive substring containment.
    Contains,
    /// Case-insensitive prefix match.
    StartsWith,
}

/// A header-match condition.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderMatch {
    /// The header name, compared case-insensitively (HTTP field names are
    /// case-insensitive per RFC 9110 §5.1).
    pub name: String,
    /// The value to match against.
    pub value: String,
    /// How `value` is compared. Default: `equals`.
    #[serde(default)]
    pub mode: HeaderMatchMode,
}

impl HeaderMatch {
    /// Whether `actual` satisfies this condition. Comparison is ASCII
    /// case-insensitive in every mode, matching header-name semantics.
    pub fn matches(&self, actual: &str) -> bool {
        match self.mode {
            HeaderMatchMode::Equals => actual.eq_ignore_ascii_case(&self.value),
            HeaderMatchMode::Contains => actual
                .to_ascii_lowercase()
                .contains(&self.value.to_ascii_lowercase()),
            // Slice before comparing so a prefix match needs no allocations.
            // `get` also rejects a non-char-boundary length, which would panic
            // under direct indexing.
            HeaderMatchMode::StartsWith => actual
                .get(..self.value.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&self.value)),
        }
    }
}

/// Day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}
