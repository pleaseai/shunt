//! Antigravity request shaping: the agent envelope and catalog model ids.
//!
//! Antigravity speaks the same Code Assist protocol the `gemini` provider
//! does — [`crate::model::gemini_request`] still builds the inner
//! `generateContent` body — but it is addressed as the Antigravity client
//! rather than as the Gemini CLI, and its catalog names models differently.
//! Both differences live here so the Code Assist path stays untouched.

use std::collections::BTreeSet;

use serde_json::{json, Value};

/// Client identity the Antigravity client sends on every request.
const ANTIGRAVITY_USER_AGENT: &str = "antigravity";
const ANTIGRAVITY_REQUEST_TYPE: &str = "agent";
/// Only Gemini ids carry an effort suffix in the Antigravity catalog.
const ANTIGRAVITY_GEMINI_PREFIX: &str = "gemini-";
/// Ordered weakest to strongest: [`nearest_published_tier`] measures distance
/// along this list, so the order is load-bearing, not cosmetic.
const ANTIGRAVITY_EFFORT_TIERS: [&str; 3] = ["low", "medium", "high"];
/// The catalog's single-id form, which names no effort of its own and takes
/// one as `thinkingConfig.thinkingLevel` instead.
const ANTIGRAVITY_TIERED_SUFFIX: &str = "tiered";
const ANTIGRAVITY_DEFAULT_TIER: &str = "medium";
const THINKING_LOW_BUDGET: u64 = 2048;
const THINKING_MEDIUM_BUDGET: u64 = 8192;
/// Bound on a rendered `sessionId`, matching the reference client's
/// `generateSessionID` (`rand.Int63n(9_000_000_000_000_000_000)` in
/// router-for-me/CLIProxyAPI, whose stable variant masks to
/// `0x7FFF_FFFF_FFFF_FFFF`). Deliberately below `i64::MAX` rather than the
/// 19-digit ceiling of `10^19`: that ceiling admits values no Antigravity
/// client ever emits, and a backend parsing the field as a signed 64-bit
/// integer would reject them.
/// Bytes of the opening user text that seed the stable session id.
const SESSION_SEED_LIMIT: usize = 4096;
const SESSION_ID_MODULUS: u64 = 9_000_000_000_000_000_000;

/// Wrap a Gemini `generateContent` request in the Antigravity *agent*
/// envelope.
///
/// The Antigravity client sends the Code Assist protocol plus its own
/// identity: `userAgent`, `requestType`, a per-request `requestId`, and a
/// `sessionId` inside the inner request. The field set mirrors
/// `geminiToAntigravity` in router-for-me/CLIProxyAPI
/// (`internal/runtime/executor/antigravity_executor_request.go`), which is the
/// only public description of the shape that client sends.
///
/// A live probe reached `200` on the daily host with this envelope and a
/// suffixed model id. It did not isolate the envelope from the host, so this
/// mirrors the client rather than asserting the backend rejects the plain
/// Code Assist envelope of [`crate::model::gemini_request::wrap_code_assist_envelope`].
pub fn wrap_antigravity_envelope(
    model: &str,
    project: &str,
    request: Value,
    request_id: &str,
    session_id: &str,
) -> Value {
    let mut request = request;
    if let Some(object) = request.as_object_mut() {
        object.insert("sessionId".to_string(), json!(session_id));
    }
    json!({
        "model": model,
        "project": project,
        "userAgent": ANTIGRAVITY_USER_AGENT,
        "requestType": ANTIGRAVITY_REQUEST_TYPE,
        "requestId": request_id,
        "request": request
    })
}

/// A fresh `requestId` for one Antigravity request, in the `agent-<uuid v4>`
/// shape the reference client uses.
pub fn antigravity_request_id() -> String {
    format!("agent-{}", uuid::Uuid::new_v4())
}

/// The `sessionId` for a *translated* Gemini request.
///
/// The backend correlates turns by this value, so it is derived from the
/// earliest user text in the history rather than drawn at random: a follow-up
/// turn in the same conversation repeats that opening text and lands on the
/// same session, mirroring `generateStableSessionID` in CLIProxyAPI. The scan
/// covers every user turn, not only the first — an opening turn that is an
/// image or a tool result alone would otherwise push every follow-up onto a
/// fresh random id. A request with no user text at all (an empty or tool-only
/// history) has nothing stable to key on and gets a random id instead of
/// colliding with every other such request.
pub fn antigravity_session_id(request: &Value) -> String {
    let seed = request
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|content| content.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|content| content.get("parts").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .find(|text| !text.is_empty());

    let digits = match seed {
        Some(text) => stable_session_digits(text),
        None => rand::random::<u64>() % SESSION_ID_MODULUS,
    };
    format!("-{digits}")
}

/// FNV-1a, spelled out rather than reached for through `DefaultHasher`: this
/// value is a session identity the backend correlates across requests, so it
/// must not change when the standard library's default hasher does. The
/// modulus keeps the rendering inside the 19 decimal digits the reference
/// client emits. Only the first [`SESSION_SEED_LIMIT`] bytes feed the hash:
/// an opening turn can carry a whole pasted file, and a bounded prefix is
/// already enough to tell sessions apart without scanning every byte on the
/// request path.
fn stable_session_digits(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes().take(SESSION_SEED_LIMIT) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash % SESSION_ID_MODULUS
}

/// The wire model id for one Antigravity request, plus the thinking level it
/// has to carry.
///
/// The two travel together because they are one decision: a `-tiered` catalog
/// id names no effort of its own, so the tier that would have been a suffix
/// moves into `request.generationConfig.thinkingConfig.thinkingLevel` instead.
/// Every other form keeps the tier in the id and sets no level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityModel {
    pub id: String,
    /// Always one of `low` / `medium` / `high` when set: the backend parses
    /// this field (an unknown value is a `400`), so an operator's unrecognised
    /// `effort` folds onto the default rather than going out verbatim the way
    /// it may in an id suffix.
    pub thinking_level: Option<&'static str>,
}

impl AntigravityModel {
    fn as_written(upstream_model: &str) -> Self {
        Self {
            id: upstream_model.to_string(),
            thinking_level: None,
        }
    }
}

/// Resolve the Antigravity catalog id for `upstream_model`, and the thinking
/// level that has to accompany it.
///
/// The Antigravity backend serves no bare Gemini slug — a bare id is a 404 on
/// the daily host and a misleading `429 RESOURCE_EXHAUSTED` on production — and
/// it publishes two different shapes of the same model, *per account*: the
/// effort baked into the id (`gemini-3.6-flash-medium`, `gemini-3.1-pro-high`)
/// and a single `-tiered` id that takes the effort as a `thinkingLevel`
/// (`gemini-3.8-flash-tiered`). Which one exists is the account's business, so
/// `catalog` — the live key set from
/// [`crate::auth::antigravity::catalog::catalog_ids`] — decides, in order:
///
/// 1. `upstream_model` is itself a catalog key → sent as written.
/// 2. `{upstream_model}-{tier}` is a key → that id, no level.
/// 3. `{upstream_model}-tiered` is a key → that id, carrying the tier as the
///    thinking level.
/// 4. Some other `{upstream_model}-{tier}` is a key → the nearest published
///    tier (see [`nearest_published_tier`]), which subsumes the Pro
///    `medium → high` clamp.
/// 5. Nothing matches → the no-catalog behaviour below.
///
/// `catalog = None` — discovery has never succeeded for this backend — is
/// exactly the pre-catalog behaviour: `{upstream_model}-{tier}` with the Pro
/// clamp. A discovery outage therefore degrades to shunt 0.40.0, never worse.
///
/// The tier itself comes from the strongest signal available, unchanged: an
/// explicit `effort` on the route or provider, the inbound request's
/// `output_config.effort`, an enabled `thinking` budget, and finally `medium`.
///
/// Ids that are not Gemini (`claude-sonnet-4-6`, `gpt-oss-120b-medium`) and ids
/// that already carry a tier or the `-tiered` marker are returned untouched.
pub fn antigravity_upstream_model(
    upstream_model: &str,
    route_effort: Option<&str>,
    request: &Value,
    catalog: Option<&BTreeSet<String>>,
) -> AntigravityModel {
    if let Some(catalog) = catalog {
        // Ahead of the shape checks below: `-tiered` is not a tier, so an id
        // the account publishes in that form would otherwise be suffixed into
        // `…-tiered-medium`, which no catalog contains.
        if catalog.contains(upstream_model) {
            tracing::debug!(
                upstream_model,
                catalog = "hit",
                form = "as-published",
                "resolved antigravity model id"
            );
            return AntigravityModel::as_written(upstream_model);
        }
    }

    let Some((tier, source)) = antigravity_effort_tier(upstream_model, route_effort, request)
    else {
        return AntigravityModel::as_written(upstream_model);
    };

    if let Some(catalog) = catalog {
        if let Some(resolved) = resolve_against_catalog(upstream_model, &tier, catalog) {
            tracing::debug!(
                upstream_model,
                resolved = %resolved.id,
                thinking_level = resolved.thinking_level.unwrap_or("-"),
                source,
                catalog = "hit",
                form = if resolved.thinking_level.is_some() {
                    "tiered"
                } else {
                    "suffix"
                },
                "resolved antigravity model id"
            );
            return resolved;
        }
    }

    let resolved = format!(
        "{upstream_model}-{}",
        clamp_tier_to_family(upstream_model, tier)
    );
    tracing::debug!(
        upstream_model,
        resolved = %resolved,
        source,
        catalog = if catalog.is_some() { "hit" } else { "miss" },
        form = "heuristic",
        "resolved antigravity model id"
    );
    AntigravityModel {
        id: resolved,
        thinking_level: None,
    }
}

/// The catalog-driven half of [`antigravity_upstream_model`], or `None` when
/// the account publishes nothing that resembles `upstream_model` — in which
/// case the caller falls back to the no-catalog heuristic rather than inventing
/// an id from an unrelated key set.
fn resolve_against_catalog(
    upstream_model: &str,
    tier: &str,
    catalog: &BTreeSet<String>,
) -> Option<AntigravityModel> {
    let exact = format!("{upstream_model}-{tier}");
    if catalog.contains(&exact) {
        return Some(AntigravityModel {
            id: exact,
            thinking_level: None,
        });
    }

    let tiered = format!("{upstream_model}-{ANTIGRAVITY_TIERED_SUFFIX}");
    if catalog.contains(&tiered) {
        return Some(AntigravityModel {
            id: tiered,
            // The suffix path may carry an operator's unrecognised tier
            // verbatim, so a future catalog tier is reachable without a
            // release. This path may not: the backend parses `thinkingLevel`
            // and rejects what it does not know, so an unknown level would
            // turn a working request into a 400.
            thinking_level: Some(fold_effort(tier).unwrap_or(ANTIGRAVITY_DEFAULT_TIER)),
        });
    }

    nearest_published_tier(upstream_model, tier, catalog).map(|id| AntigravityModel {
        id,
        thinking_level: None,
    })
}

/// The published `{upstream_model}-{tier}` id closest to `tier`.
///
/// Tiers are ordered `low < medium < high`, and the nearest published one by
/// that distance wins; a tie breaks upward, because under-serving a request is
/// the more surprising of the two failures. This is the general form of the
/// hard-coded Pro clamp: an account whose Pro model is published as `-low` and
/// `-high` resolves a derived `medium` to `high` because it read the catalog,
/// not because Pro was special-cased. A `tier` outside the published
/// vocabulary has no position to measure from, so it yields `None` and the
/// caller falls back.
fn nearest_published_tier(
    upstream_model: &str,
    tier: &str,
    catalog: &BTreeSet<String>,
) -> Option<String> {
    let wanted = ANTIGRAVITY_EFFORT_TIERS
        .iter()
        .position(|published| *published == tier)?;
    ANTIGRAVITY_EFFORT_TIERS
        .iter()
        .enumerate()
        .map(|(index, published)| (index, format!("{upstream_model}-{published}")))
        .filter(|(_, id)| catalog.contains(id))
        .min_by_key(|(index, _)| (index.abs_diff(wanted), usize::MAX - *index))
        .map(|(_, id)| id)
}

/// The tier the request asks for, plus the signal that decided it, or `None`
/// when `upstream_model` must be sent as written. Unclamped and unmatched
/// against any catalog — [`antigravity_upstream_model`] does both.
fn antigravity_effort_tier(
    upstream_model: &str,
    route_effort: Option<&str>,
    request: &Value,
) -> Option<(String, &'static str)> {
    if !upstream_model.starts_with(ANTIGRAVITY_GEMINI_PREFIX)
        || ends_with_tier(upstream_model, ANTIGRAVITY_TIERED_SUFFIX)
        || ANTIGRAVITY_EFFORT_TIERS
            .iter()
            .any(|tier| ends_with_tier(upstream_model, tier))
    {
        return None;
    }

    let (tier, source) = if let Some(effort) = route_effort {
        // Operator intent wins over every request signal, but the *value* is
        // normalized the same way a request's is: `xhigh` and `max` name
        // levels the Antigravity catalog has never published, and appending
        // one verbatim only produces an id the backend cannot serve.
        let tier = fold_effort(effort)
            .map(str::to_string)
            // No allowlist for the operator: the backend catalog is the
            // authority on which tiers exist, so a tier shunt has not heard of
            // must be reachable without a release. It still goes out in the
            // catalog's own spelling — trimmed and lower-cased — so `High`
            // names the published tier rather than a `-High` id.
            .unwrap_or_else(|| normalize_effort(effort));
        (tier, "config")
    } else if let Some(effort) = request
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
    {
        // Unlike the operator's own `effort`, this value comes from the
        // inbound request, so an unrecognised one falls back to the default
        // rather than being pasted into the upstream model id.
        let tier = fold_effort(effort).unwrap_or(ANTIGRAVITY_DEFAULT_TIER);
        (tier.to_string(), "output_config")
    } else if request.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled") {
        // An enabled block that names no budget resolves to the same default
        // the translated request carries in `thinkingConfig.thinkingBudget`,
        // so the tier and the budget describe one request rather than two.
        let budget = request
            .pointer("/thinking/budget_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(crate::model::gemini_request::DEFAULT_THINKING_BUDGET);
        (thinking_budget_tier(budget).to_string(), "thinking")
    } else {
        (ANTIGRAVITY_DEFAULT_TIER.to_string(), "default")
    };

    // Returned raw: the Pro clamp is one *heuristic* answer to "this tier may
    // not be published", and a live catalog answers the same question with
    // evidence. Applying it here would pre-empt the catalog.
    Some((tier, source))
}

fn ends_with_tier(model: &str, tier: &str) -> bool {
    model
        .strip_suffix(tier)
        .is_some_and(|head| head.ends_with('-'))
}

/// Normalize one effort level — from a route/provider `effort` or from a
/// request's `output_config.effort` — onto the tiers Antigravity publishes.
///
/// Claude Code sends `low|medium|high|xhigh|max`; the catalog stops at `high`,
/// so the two levels above it fold onto it. Matching is case-insensitive and
/// ignores surrounding whitespace, so a hand-written `High` is the published
/// tier rather than a level shunt has never seen. `None` means the level is
/// outside that vocabulary; each caller decides what to do with it, since an
/// operator naming a tier shunt has not heard of and a client sending noise
/// deserve different answers.
fn fold_effort(effort: &str) -> Option<&'static str> {
    match normalize_effort(effort).as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// The catalog spells its tiers in lower case with no padding.
fn normalize_effort(effort: &str) -> String {
    effort.trim().to_ascii_lowercase()
}

/// `thinking.budget_tokens` is the only quantitative signal a client that does
/// not send `output_config` gives about how much reasoning it wants. The
/// thresholds are the same ones Claude Code's own tiers land on.
fn thinking_budget_tier(budget: u64) -> &'static str {
    if budget <= THINKING_LOW_BUDGET {
        "low"
    } else if budget <= THINKING_MEDIUM_BUDGET {
        "medium"
    } else {
        "high"
    }
}

/// The Pro family is published as `-low` and `-high` only, so a derived (or
/// pinned) `medium` would 404. Flash keeps all three.
///
/// The family is read from the id's `-`-separated segments, so a
/// `gemini-3-pro-preview`-style variant is still Pro while an id that merely
/// contains the letters (`-prod`, `-prompt`) is not. It is deliberately not a
/// suffix check: `high` is published in every family and `medium` is not in
/// Pro, so mis-reading a Pro variant as Flash costs a `404` where the reverse
/// only costs a tier.
fn clamp_tier_to_family(upstream_model: &str, tier: String) -> String {
    let is_pro = upstream_model.split('-').any(|segment| segment == "pro");
    if tier == "medium" && is_pro {
        return "high".to_string();
    }
    tier
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_antigravity_envelope_carries_the_agent_identity() {
        let wrapped = wrap_antigravity_envelope(
            "gemini-3.8-flash-medium",
            "test-proj-789",
            json!({ "contents": [] }),
            "agent-6f0c9d4e-0000-4000-8000-000000000000",
            "-1234567890123456789",
        );

        assert_eq!(wrapped["model"], "gemini-3.8-flash-medium");
        assert_eq!(wrapped["project"], "test-proj-789");
        assert_eq!(wrapped["userAgent"], "antigravity");
        assert_eq!(wrapped["requestType"], "agent");
        assert_eq!(
            wrapped["requestId"],
            "agent-6f0c9d4e-0000-4000-8000-000000000000"
        );
        // The session id rides *inside* the inner request, not beside it.
        assert_eq!(wrapped["request"]["sessionId"], "-1234567890123456789");
        assert!(wrapped["request"].get("contents").is_some());
    }

    #[test]
    fn a_request_id_is_a_fresh_agent_prefixed_uuid() {
        let first = antigravity_request_id();
        let second = antigravity_request_id();

        assert!(first.starts_with("agent-"), "{first}");
        assert_ne!(first, second, "requestId is per request");
        uuid::Uuid::parse_str(first.trim_start_matches("agent-")).expect("uuid v4 suffix");
    }

    #[test]
    fn a_session_id_is_stable_for_the_same_opening_user_text() {
        // The backend correlates a conversation by this value, so the same
        // opening turn must reach the same session on every follow-up request.
        let request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "refactor the parser"}]},
                {"role": "model", "parts": [{"text": "on it"}]},
            ]
        });
        let other = json!({
            "contents": [{"role": "user", "parts": [{"text": "refactor the lexer"}]}]
        });

        let session_id = antigravity_session_id(&request);
        assert_eq!(session_id, antigravity_session_id(&request));
        assert_ne!(session_id, antigravity_session_id(&other));

        let digits = session_id
            .strip_prefix('-')
            .expect("a session id is a leading dash then decimal digits");
        assert!(digits.chars().all(|character| character.is_ascii_digit()));
        assert!(digits.len() <= 19, "{digits} is longer than 19 digits");
    }

    #[test]
    fn a_session_id_always_fits_in_a_signed_64_bit_integer() {
        // The reference client draws from `Int63n`, so every session id it
        // emits is a positive i64. A 10^19 modulus would have kept the string
        // inside 19 digits while still producing values above `i64::MAX`,
        // which a backend parsing this field as a signed integer rejects.
        let mut seeds: Vec<String> = (0..256).map(|index| format!("turn {index}")).collect();
        seeds.extend([
            String::new(),
            "\u{ac00}\u{b098}\u{b2e4}".to_string(),
            "x".repeat(4096),
        ]);

        for seed in &seeds {
            let request = json!({
                "contents": [{"role": "user", "parts": [{"text": seed}]}]
            });
            let session_id = antigravity_session_id(&request);
            // The id itself stays out of these messages: CodeQL reads a
            // formatted `session_id` as sensitive data reaching a log sink.
            let digits = session_id
                .strip_prefix('-')
                .expect("a session id must start with a dash");
            assert!(digits.len() <= 19, "a session id is at most 19 digits");
            digits
                .parse::<i64>()
                .unwrap_or_else(|error| panic!("a session id must parse as i64: {error}"));
        }

        // The random fallback is bounded by the same modulus.
        for _ in 0..256 {
            let session_id = antigravity_session_id(&json!({ "contents": [] }));
            let digits = session_id.strip_prefix('-').expect("leading dash");
            assert!(
                digits.len() <= 19,
                "a random session id is at most 19 digits"
            );
            digits.parse::<i64>().expect("random ids parse as i64");
        }
    }

    #[test]
    fn a_session_id_skips_leading_non_user_turns() {
        // Only the first *user* turn seeds the id; a model turn ahead of it
        // (a replayed history) must not.
        let with_model_first = json!({
            "contents": [
                {"role": "model", "parts": [{"text": "hello"}]},
                {"role": "user", "parts": [{"text": "refactor the parser"}]},
            ]
        });
        let user_only = json!({
            "contents": [{"role": "user", "parts": [{"text": "refactor the parser"}]}]
        });

        assert_eq!(
            antigravity_session_id(&with_model_first),
            antigravity_session_id(&user_only)
        );
    }

    #[test]
    fn a_session_id_survives_an_image_only_opening_turn() {
        // An opening turn that is an image (or a tool result) alone carries no
        // text; the first user *text* in the history seeds the id instead, so
        // follow-ups still correlate rather than each drawing a random id.
        let image_first = json!({
            "contents": [
                {"role": "user", "parts": [{"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]},
                {"role": "model", "parts": [{"text": "I see a chart"}]},
                {"role": "user", "parts": [{"text": "refactor the parser"}]},
            ]
        });
        // An empty text part is not a seed either: the scan continues past
        // it rather than stopping and falling back to a random id.
        let empty_text_first = json!({
            "contents": [
                {"role": "user", "parts": [{"text": ""}, {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]},
                {"role": "user", "parts": [{"text": "refactor the parser"}]},
            ]
        });
        let user_only = json!({
            "contents": [{"role": "user", "parts": [{"text": "refactor the parser"}]}]
        });

        assert_eq!(
            antigravity_session_id(&image_first),
            antigravity_session_id(&image_first)
        );
        assert_eq!(
            antigravity_session_id(&image_first),
            antigravity_session_id(&user_only)
        );
        assert_eq!(
            antigravity_session_id(&empty_text_first),
            antigravity_session_id(&user_only)
        );
    }

    #[test]
    fn a_session_id_without_user_text_is_random_rather_than_shared() {
        // Nothing stable to key on, so every such request gets its own session
        // instead of all of them colliding on one.
        let request = json!({ "contents": [] });

        let first = antigravity_session_id(&request);
        assert_ne!(first, antigravity_session_id(&request));
        let digits = first.strip_prefix('-').expect("leading dash");
        assert!(digits.chars().all(|character| character.is_ascii_digit()));
        assert!(digits.len() <= 19, "{digits} is longer than 19 digits");
    }

    /// The no-catalog resolution — discovery never succeeded for this backend.
    /// Asserts the level is unset every time, because each id this path
    /// produces carries its tier in the id itself.
    fn without_catalog(
        upstream_model: &str,
        route_effort: Option<&str>,
        request: &Value,
    ) -> String {
        let resolved = antigravity_upstream_model(upstream_model, route_effort, request, None);
        assert_eq!(
            resolved.thinking_level, None,
            "{upstream_model} keeps its tier in the id without a catalog"
        );
        resolved.id
    }

    fn catalog(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn ids_that_need_no_effort_suffix_are_sent_as_written() {
        // Rule 1: non-Gemini catalog ids carry their tier in the published
        // name (or have none), and a hand-written suffix is the operator's.
        let no_signals = json!({});
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b-medium",
            "gemini-3.8-flash-medium",
            "gemini-3.1-pro-high",
            "gemini-3.6-flash-low",
            // `-tiered` names no effort of its own, so appending one would
            // build `…-tiered-medium`, an id no catalog contains.
            "gemini-3.8-flash-tiered",
        ] {
            assert_eq!(
                without_catalog(model, None, &no_signals),
                model,
                "{model} must be sent as written"
            );
            assert_eq!(
                without_catalog(model, Some("high"), &no_signals),
                model,
                "{model} must be sent as written even with a configured effort"
            );
        }
    }

    #[test]
    fn a_configured_effort_pins_the_suffix() {
        // Rule 2: explicit operator intent wins over every request signal —
        // here over an `output_config.effort` that says something else.
        let request = json!({"output_config": {"effort": "low"}});
        assert_eq!(
            without_catalog("gemini-3.6-flash", Some("high"), &request),
            "gemini-3.6-flash-high"
        );

        // Winning does not mean escaping normalization: `xhigh` and `max` are
        // levels the catalog has never published, so they fold onto `high`
        // exactly as a request's would. A level shunt does not know is passed
        // through untouched, so an operator can name a future tier without
        // waiting for a release.
        for (effort, expected) in [
            ("low", "gemini-3.6-flash-low"),
            ("medium", "gemini-3.6-flash-medium"),
            ("high", "gemini-3.6-flash-high"),
            ("xhigh", "gemini-3.6-flash-high"),
            ("max", "gemini-3.6-flash-high"),
            ("extra-low", "gemini-3.6-flash-extra-low"),
            // Spelling is normalized before either branch: a hand-written
            // `High` is the published tier, not a `-High` id the backend
            // cannot serve, and an unknown level goes out in catalog case.
            ("High", "gemini-3.6-flash-high"),
            (" MAX ", "gemini-3.6-flash-high"),
            ("Extra-Low", "gemini-3.6-flash-extra-low"),
        ] {
            assert_eq!(
                without_catalog("gemini-3.6-flash", Some(effort), &json!({})),
                expected,
                "configured effort {effort}"
            );
        }
    }

    #[test]
    fn the_request_effort_decides_when_nothing_is_configured() {
        // Rule 3: Claude Code sends low|medium|high|xhigh|max; the catalog
        // stops at high, so the two levels above it fold onto it. A level from
        // outside that vocabulary is *not* passed through here — this value
        // comes from the inbound request, so it falls back to the default
        // rather than reaching the upstream model id.
        for (effort, expected) in [
            ("low", "gemini-3.6-flash-low"),
            ("medium", "gemini-3.6-flash-medium"),
            ("high", "gemini-3.6-flash-high"),
            ("xhigh", "gemini-3.6-flash-high"),
            ("max", "gemini-3.6-flash-high"),
            ("extra-low", "gemini-3.6-flash-medium"),
        ] {
            let request = json!({"output_config": {"effort": effort}});
            assert_eq!(
                without_catalog("gemini-3.6-flash", None, &request),
                expected,
                "output_config.effort {effort}"
            );
        }
    }

    #[test]
    fn a_thinking_budget_decides_when_no_effort_is_sent() {
        // Rule 4, at both threshold boundaries. An enabled block with no
        // budget is not "as much as possible": the translated request sends
        // the 1024 default in `thinkingConfig.thinkingBudget`, so the tier has
        // to describe that same budget, which lands in `low`.
        for (budget, expected) in [
            (Some(2048), "gemini-3.6-flash-low"),
            (Some(2049), "gemini-3.6-flash-medium"),
            (Some(8192), "gemini-3.6-flash-medium"),
            (Some(8193), "gemini-3.6-flash-high"),
            (None, "gemini-3.6-flash-low"),
        ] {
            let thinking = match budget {
                Some(budget) => json!({"type": "enabled", "budget_tokens": budget}),
                None => json!({"type": "enabled"}),
            };
            let request = json!({"thinking": thinking});
            assert_eq!(
                without_catalog("gemini-3.6-flash", None, &request),
                expected,
                "thinking budget {budget:?}"
            );
        }

        // A disabled thinking block is not a signal.
        let disabled = json!({"thinking": {"type": "disabled", "budget_tokens": 100}});
        assert_eq!(
            without_catalog("gemini-3.6-flash", None, &disabled),
            "gemini-3.6-flash-medium"
        );
    }

    #[test]
    fn a_bare_gemini_id_with_no_signals_at_all_resolves_to_medium() {
        // Rule 5. The bare id itself is never served — a 404 on the daily host
        // and a misleading 429 on production — so there is no "leave it alone"
        // option here.
        assert_eq!(
            without_catalog("gemini-3.6-flash", None, &json!({})),
            "gemini-3.6-flash-medium"
        );
    }

    #[test]
    fn the_pro_family_has_no_medium_tier() {
        // Rule 6: Pro is published as `-low` / `-high` only, so a medium from
        // any source clamps up rather than 404ing.
        assert_eq!(
            without_catalog("gemini-3.1-pro", None, &json!({})),
            "gemini-3.1-pro-high"
        );
        let request = json!({"output_config": {"effort": "medium"}});
        assert_eq!(
            without_catalog("gemini-3.1-pro", None, &request),
            "gemini-3.1-pro-high"
        );
        assert_eq!(
            without_catalog(
                "gemini-3.1-pro",
                None,
                &json!({"thinking": {"type": "enabled", "budget_tokens": 4096}})
            ),
            "gemini-3.1-pro-high"
        );
        // The family is a segment of the id, not a suffix: a Pro variant
        // still clamps, and an id that only contains the letters does not.
        assert_eq!(
            without_catalog("gemini-3-pro-preview", None, &json!({})),
            "gemini-3-pro-preview-high"
        );
        assert_eq!(
            without_catalog("gemini-3.8-flash-prod", None, &json!({})),
            "gemini-3.8-flash-prod-medium"
        );
        // Low still reaches Pro, and Flash keeps its medium.
        assert_eq!(
            without_catalog(
                "gemini-3.1-pro",
                None,
                &json!({"output_config": {"effort": "low"}})
            ),
            "gemini-3.1-pro-low"
        );
        assert_eq!(
            without_catalog("gemini-3.8-flash", None, &json!({})),
            "gemini-3.8-flash-medium"
        );
    }

    #[test]
    fn a_published_suffix_id_beats_the_tiered_form() {
        // An account served both shapes gets the one that names the tier in
        // the id: it needs no second field to be right, and it is what shunt
        // sent before the catalog existed.
        let both = catalog(&["gemini-3.6-flash-medium", "gemini-3.6-flash-tiered"]);
        assert_eq!(
            antigravity_upstream_model("gemini-3.6-flash", None, &json!({}), Some(&both)),
            AntigravityModel {
                id: "gemini-3.6-flash-medium".to_string(),
                thinking_level: None,
            }
        );
    }

    #[test]
    fn a_tiered_only_catalog_moves_the_tier_into_the_thinking_level() {
        // Today's live failure: the account publishes `gemini-3.8-flash-tiered`
        // and no `gemini-3.8-flash-medium`, so 0.40.0's hard-coded suffix
        // 404s. The tier still has to travel — as `thinkingLevel`.
        let tiered = catalog(&["gemini-3.8-flash-tiered", "gemini-3.6-flash-medium"]);
        for (effort, expected_level) in [
            (None, "medium"),
            (Some("low"), "low"),
            (Some("high"), "high"),
            // The backend parses this field, so an operator's unrecognised
            // level folds onto the default instead of going out verbatim and
            // turning a working request into a 400.
            (Some("extra-low"), "medium"),
            (Some("max"), "high"),
        ] {
            assert_eq!(
                antigravity_upstream_model("gemini-3.8-flash", effort, &json!({}), Some(&tiered)),
                AntigravityModel {
                    id: "gemini-3.8-flash-tiered".to_string(),
                    thinking_level: Some(expected_level),
                },
                "configured effort {effort:?}"
            );
        }
    }

    #[test]
    fn an_id_the_catalog_publishes_verbatim_is_never_reshaped() {
        // Whatever the operator pinned, a key the account actually publishes
        // is already a wire id — suffixing it can only invent one that is not.
        let published = catalog(&[
            "claude-sonnet-4-6",
            "gemini-3.8-flash-tiered",
            "gemini-3.6-flash-medium",
        ]);
        for model in [
            "claude-sonnet-4-6",
            "gemini-3.8-flash-tiered",
            "gemini-3.6-flash-medium",
        ] {
            assert_eq!(
                antigravity_upstream_model(model, Some("high"), &json!({}), Some(&published)),
                AntigravityModel {
                    id: model.to_string(),
                    thinking_level: None,
                },
                "{model} is already a catalog id"
            );
        }
    }

    #[test]
    fn a_missing_tier_resolves_to_the_nearest_one_the_account_publishes() {
        // The general form of the Pro clamp, now driven by evidence: Pro is
        // published as low/high, so a derived medium is equidistant and breaks
        // upward. The same rule serves a Flash family published low-only.
        let pro = catalog(&["gemini-3.1-pro-low", "gemini-3.1-pro-high"]);
        assert_eq!(
            antigravity_upstream_model("gemini-3.1-pro", None, &json!({}), Some(&pro)).id,
            "gemini-3.1-pro-high"
        );
        assert_eq!(
            antigravity_upstream_model("gemini-3.1-pro", Some("low"), &json!({}), Some(&pro)).id,
            "gemini-3.1-pro-low"
        );

        let low_only = catalog(&["gemini-3.9-flash-low"]);
        assert_eq!(
            antigravity_upstream_model(
                "gemini-3.9-flash",
                Some("high"),
                &json!({}),
                Some(&low_only)
            )
            .id,
            "gemini-3.9-flash-low"
        );
    }

    #[test]
    fn a_catalog_that_knows_nothing_about_the_model_falls_back_to_the_heuristic() {
        // A stale or unrelated key set must not make the request worse than a
        // discovery outage would: an empty catalog and no catalog at all have
        // to land on the same id, Pro clamp included.
        for irrelevant in [
            catalog(&[]),
            catalog(&["claude-sonnet-4-6", "gpt-oss-120b-medium"]),
        ] {
            for (model, expected) in [
                ("gemini-3.6-flash", "gemini-3.6-flash-medium"),
                ("gemini-3.1-pro", "gemini-3.1-pro-high"),
            ] {
                assert_eq!(
                    antigravity_upstream_model(model, None, &json!({}), Some(&irrelevant)),
                    AntigravityModel {
                        id: expected.to_string(),
                        thinking_level: None,
                    },
                    "{model} against an irrelevant catalog"
                );
                assert_eq!(
                    without_catalog(model, None, &json!({})),
                    expected,
                    "{model} without a catalog"
                );
            }
        }
    }
}
