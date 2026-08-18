//! The credential-slot enumeration: every place shunt *accepts* one of its own
//! credentials, every place it *forwards* headers to a third party, and the one
//! rule that has to hold between them.
//!
//! **The mirror invariant.** If any accept site would authenticate value `V`
//! presented in slot `S`, then every forward site must remove `V` from `S`
//! before the request leaves shunt. The strip predicate is the mirror image of
//! the accept predicate — not an approximation of it, and not a per-request
//! boolean. Four consecutive fixes landed the same defect (#352, #357, #361,
//! #356): each widened an accept predicate, or added a credential kind, and
//! left one enumeration of a slot behind. This module exists so there is a
//! single enumeration for a forward site to consult, and
//! [`ShuntCredentials::from_state`] is the single wiring point, so a new
//! credential kind added to [`AppState`] cannot be silently missed by a
//! forward site that hand-rolled its own field list.
//!
//! ## Slots
//!
//! | Slot | Owner | How it is cleared |
//! | --- | --- | --- |
//! | `authorization` | shared with the caller's own upstream credential | **by value** ([`ShuntCredentials::strip_consumed_slots`]) |
//! | `x-api-key` | shared with the caller's own upstream credential | **by value** ([`ShuntCredentials::strip_consumed_slots`]) |
//! | `x-shunt-token`, `x-shunt-admin-token`, `x-shunt-inbound-client`, `cookie`, plus whatever `[server.auth] header` / `[server.admin] header` name | reserved by shunt | **by name** ([`ShuntCredentials::strip_reserved_slots`]) |
//!
//! ## Accept sites
//!
//! **This list is exhaustive for *header* slots**, which is the scope the
//! invariant is about: a header is the only channel a forward site can copy
//! from the caller's request into an upstream request. shunt accepts its own
//! credentials through non-header channels too; those are listed at the end
//! with why they cannot reach an upstream.
//!
//! - [`crate::auth::inbound::InboundAuth::authenticate`] — the configured
//!   `[server.auth] header`, raw.
//! - [`crate::auth::inbound::InboundAuth::authenticate_bearer`] — that header
//!   raw, plus the `Authorization: Bearer` payload.
//! - [`crate::auth::inbound::InboundAuth::authenticate_client`] — that header
//!   raw, plus the `Authorization: Bearer` payload, plus `x-api-key` raw.
//! - [`crate::gateway::GatewayAuth::authenticate_bearer`] — the
//!   `Authorization: Bearer` payload; `authenticate_token` the bare value,
//!   reached in production only through that bearer path and through
//!   [`crate::auth::inbound::consumed_by`].
//! - [`crate::admin::AdminAuth::authenticate_credential`] — the configured
//!   `[server.admin] header` raw **and** `x-api-key` raw, over every
//!   `[server.admin]` credential (`write_keys`, `read_keys`, and the legacy
//!   `tokens_env`/`tokens_file` pairs alike).
//! - `crate::admin::authenticate` — falls back to `session_cookie`, which
//!   accepts a **write-tier** `shunt_admin_session` out of the `cookie` header
//!   when no credential header matched. This is why `cookie` is in
//!   [`RESERVED_SLOTS`]; it was missing from the first version of this
//!   enumeration and two of the three forward sites relayed it.
//!
//! Their callers — `discovery`, `usage`, `oauth_usage`, `codex_analytics`,
//! `codex_endpoint`, `proxy::failover`, `gateway::telemetry_ingest`,
//! `gateway::managed`, `gateway::spend::api`, `admin` — add no header slot of
//! their own; they only choose which of the predicates above to run.
//!
//! ### Non-header accept channels (not a forwarding risk)
//!
//! shunt also accepts values it minted, or admin credentials, out of **form
//! bodies and query strings**: `admin::login_submit` (a write-tier admin
//! credential in a form field, via `authenticate_login_token`),
//! `gateway::oauth`, `gateway::device`, `gateway::idp`, `admin::oidc`, and
//! `auth::callback`. None of them can leak the way a header can, and the reason
//! is structural rather than a rule anyone has to remember: no forward site
//! copies an inbound body or an inbound query string into an outbound request.
//! Every upstream URL is *rebuilt* from config (`responses_url` and friends),
//! and the request body a forward site sends is the caller's inference payload,
//! which never carries these values. They are recorded here so the enumeration
//! is honest about what it does and does not cover, not because they need a
//! strip.
//!
//! ## Forward sites
//!
//! 1. [`crate::proxy::failover::check_inbound_auth`] (reserved names) plus
//!    [`crate::proxy::failover::headers_for_route`] (by value, on the
//!    same-origin passthrough branch). Every inference adapter receives only
//!    what `headers_for_route` produced, so the pair is the single choke point
//!    for `/v1/messages`.
//! 2. `crate::discovery::upstream::upstream_headers`, `AuthMode::Passthrough`
//!    branch — builds a fresh map holding only the two shared slots, each
//!    judged by value.
//! 3. [`crate::adapters::responses::inbound::passthrough_request_headers`] —
//!    the inbound Codex endpoint, which relays the client's headers verbatim
//!    minus a strip list plus [`ShuntCredentials::strip_reserved_slots`].
//!
//! Deliberately *not* forward sites, because they build their outbound header
//! map from an allowlist rather than from the caller's: `telemetry_ingest`'s
//! relay (content-type/content-encoding only), `adapters::responses::request`,
//! and `adapters::responses::codex_ws::connect`.
//!
//! ## Tripwire
//!
//! `tests::every_header_producing_site_is_classified` walks `src/**/*.rs` and
//! asserts the set of files that either bulk-apply a header map to an outbound
//! request *or* declare a function returning `HeaderMap` matches a hard-coded
//! allowlist, so a new relay path has to be classified rather than merely
//! compile.
//!
//! The type-signature half is what makes it useful. Matching only the bulk
//! application idioms left **both** hand-rolled forward sites invisible —
//! `discovery/upstream.rs` builds its map in a `request.header(k, v)` loop, and
//! `proxy/failover.rs` returns `headers.clone()`/`base.clone()` — so a new site
//! written by copying either one escaped detection entirely. A site that
//! *produces* a `HeaderMap` is caught however it builds one. (`.header(`,
//! `.insert(`, and `HeaderMap::new()` were measured as alternatives and are
//! unusable: 38, 71, and 24 files.)
//!
//! Residual hole, narrower than before but still real: a site that mutates a
//! request in place and never returns a `HeaderMap` — the shape
//! `codex_ws::connect` has — is caught today only by the extend-into-`headers_mut`
//! pattern, and a different in-place idiom would not be. Files ending in
//! `tests.rs` are skipped, so a fixture that returns a `HeaderMap` does not
//! have to be allowlisted; in-file `#[cfg(test)] mod tests` helpers are not
//! skipped and are allowlisted as noise instead.
//!
//! Considered and deferred: wrapping the outbound map in a `SanitizedHeaders`
//! newtype that only these methods can produce, which would make the strip a
//! compile-time obligation rather than a convention a tripwire polices. It is a
//! real improvement, but the three sites hand their map to different HTTP
//! clients (`reqwest::RequestBuilder::headers`, a per-key loop, and a
//! tungstenite request), so the churn is larger than this change should carry.

use axum::http::{HeaderMap, HeaderName};

use crate::{admin::AdminAuth, config::AdminKeyring, gateway::GatewayAuth, server::AppState};

use super::inbound::{authorization_consumed_by, consumed_by, ConsumedBy, InboundAuth};

/// The two slot names, declared individually because the strip code has to name
/// them and `authorization_consumed_by` hardcodes `"authorization"` internally.
/// [`SHARED_SLOTS`] is derived from these rather than the other way round: an
/// index into the array would survive a reorder and then silently strip the
/// *other* slot from the one that was judged.
const AUTHORIZATION: &str = "authorization";
const API_KEY: &str = "x-api-key";

/// The two slots shunt shares with the caller's own upstream credential.
/// Values here are stripped **by value**, never by slot name: an
/// `apiKeyHelper` fills both with the same value, so either can hold a shunt
/// credential beside a genuine upstream credential in the other.
pub(crate) const SHARED_SLOTS: [&str; 2] = [AUTHORIZATION, API_KEY];

/// Header names shunt reserves whatever `[server.auth]`/`[server.admin]` are
/// configured to. Removed unconditionally — even on an ungated endpoint — so
/// the documented guarantee holds without depending on config: none of these
/// is ever a legitimate upstream header, so removing a name a client sent
/// cannot break a legitimate relay.
///
/// `cookie` is here for the same reason and needs its own justification, since
/// unlike the `x-shunt-*` names it *is* a standard header a caller might expect
/// to reach an upstream. Two facts make removing the whole header correct.
/// First, it is an accept slot: `admin::authenticate` falls back to
/// `session_cookie`, which reads a write-tier `shunt_admin_session` out of
/// `cookie`, and two of the three forward sites relay the header verbatim.
/// Second, shunt keeps no cookie jar — `Cargo.toml` builds reqwest **without**
/// the `cookies` feature and nothing in `src/` constructs a `cookie_store` or
/// `cookie_provider` — so shunt never participates in upstream edge or affinity
/// cookies (`__cf_bm`, `cf_clearance`) and dropping the header costs an
/// upstream nothing it was relying on. The mirror direction already makes this
/// call: `PASSTHROUGH_STRIP_RESPONSE_HEADERS` strips `set-cookie`/`set-cookie2`
/// on the way back for exactly the same reason.
///
/// Whole-header removal is deliberate over a surgical `shunt_admin_session=`
/// pair parser: a parser would have to track `session_cookie`'s own parse
/// (prefix, `;` splitting, trimming, empty-value filtering) and would
/// reintroduce precisely the accept/strip drift this module exists to
/// eliminate. The cost is that a benign `cookie: theme=dark` is dropped too;
/// that over-strip is intended and is asserted explicitly in the tests.
pub(crate) const RESERVED_SLOTS: [&str; 4] = [
    "x-shunt-token",
    "x-shunt-admin-token",
    "x-shunt-inbound-client",
    "cookie",
];

/// Everything a forward site needs to recognize one of shunt's own inbound
/// credentials: the three credential tables to check values against, and the
/// two configurable header names to clear by name.
///
/// `Copy`, so a site can take it by value and still hand it to a helper.
///
/// Deliberately **not** `Default`, and the fields are private. An all-`None`
/// value strips nothing at all — `strip_consumed_slots` matches no credential
/// and both `if let Some(name)` arms of `strip_reserved_slots` are skipped — so
/// a `Default` is a placeholder that compiles and silently disables the whole
/// boundary. [`Self::from_state`] is therefore the only way to build one
/// outside tests, which also means the production callers must pass the real
/// request state rather than something a test could stub out from underneath
/// them.
#[derive(Clone, Copy)]
pub(crate) struct ShuntCredentials<'a> {
    gateway_auth: Option<&'a GatewayAuth>,
    static_auth: Option<&'a InboundAuth>,
    admin_credentials: Option<&'a AdminKeyring>,
    /// `[server.auth] header`, when configured.
    static_header: Option<&'a HeaderName>,
    /// `[server.admin] header`, when configured.
    admin_header: Option<&'a HeaderName>,
}

impl<'a> ShuntCredentials<'a> {
    /// The single wiring point from request state, and the only production
    /// constructor. Forward sites call this rather than reading `AppState`
    /// fields themselves, so adding a credential kind is one edit here instead
    /// of an edit per site that some future change can forget.
    pub(crate) fn from_state(state: &'a AppState) -> Self {
        Self {
            gateway_auth: state.gateway_auth.as_deref(),
            static_auth: state.inbound_auth.as_deref(),
            admin_credentials: state.admin_auth.as_deref().map(AdminAuth::credentials),
            static_header: state.inbound_auth.as_deref().map(InboundAuth::header),
            admin_header: state.admin_auth.as_deref().map(AdminAuth::header),
        }
    }

    /// Build a value directly from credential tables, for tests that exercise a
    /// strip predicate without standing up an `AppState`. Test-only on purpose:
    /// production has exactly one constructor. Same pattern as
    /// [`crate::auth::inbound::is_consumed_by_shunt`].
    ///
    /// The two configured header names are always `None` here — a caller that
    /// needs them is exercising `strip_reserved_slots`, which only
    /// `from_state` can wire correctly.
    #[cfg(test)]
    pub(crate) fn for_test(
        gateway_auth: Option<&'a GatewayAuth>,
        static_auth: Option<&'a InboundAuth>,
        admin_credentials: Option<&'a AdminKeyring>,
    ) -> Self {
        Self {
            gateway_auth,
            static_auth,
            admin_credentials,
            static_header: None,
            admin_header: None,
        }
    }

    /// Clear every slot shunt reserves by *name*: the three fixed
    /// [`RESERVED_SLOTS`], the configured `[server.auth] header`, and the
    /// configured `[server.admin] header`.
    ///
    /// The two configured headers are treated asymmetrically, deliberately, and
    /// this asymmetry is the pre-existing behavior of
    /// `proxy::failover::check_inbound_auth` (see the "Caveat for
    /// `[server.auth] header = \"authorization\"`" paragraph of
    /// `docs/m4-inbound-auth.md` §2):
    ///
    /// - `static_header` is removed **always**, even when an operator pointed
    ///   it at one of the [`SHARED_SLOTS`]. Over-stripping is the accepted
    ///   cost: `InboundAuth::authenticate_client` gates on that slot's *whole*
    ///   unprefixed value, so deciding per value at the gate would forward a
    ///   slot the gate had already accepted under some configurations. The
    ///   consequence for the operator is that pointing `header` at a shared
    ///   slot costs callers that slot on inference; the default dedicated
    ///   `x-shunt-token` avoids the collision entirely.
    /// - `admin_header` is removed **only when it is not** one of the
    ///   [`SHARED_SLOTS`]. Dropping a shared slot outright for the admin header
    ///   would delete a genuine caller credential sight unseen on a passthrough
    ///   route, so that case is handled by value instead, in
    ///   [`Self::strip_consumed_slots`], which is the exact mirror of
    ///   `AdminAuth::authenticate_credential` either way.
    ///
    /// `HeaderName` lowercase-normalizes on construction, so comparing against
    /// the lowercase names above is already case-insensitive.
    pub(crate) fn strip_reserved_slots(&self, headers: &mut HeaderMap) {
        for name in RESERVED_SLOTS {
            headers.remove(name);
        }
        if let Some(name) = self.static_header {
            headers.remove(name);
        }
        if let Some(name) = self.admin_header {
            if !SHARED_SLOTS.contains(&name.as_str()) {
                headers.remove(name);
            }
        }
    }

    /// Clear each of the [`SHARED_SLOTS`] that holds one of shunt's own
    /// credentials, judging every slot independently by the value it carries —
    /// never by whether *some* slot in the request authenticated the caller, so
    /// a genuine upstream credential in one slot survives a shunt credential in
    /// the other.
    ///
    /// `authorization` is evaluated in **both** shapes it can carry a gate
    /// credential in (the `Bearer` payload and the entire raw value) because
    /// both `[server.auth] header` and `[server.admin] header` are free-form
    /// names an operator may point at `authorization`, in which case the accept
    /// predicate reads the whole unprefixed value.
    ///
    /// Both slots are judged over **every value**, not just the first. A slot is
    /// a list, and forward site 1 forwards a clone of the caller's map, so a
    /// shunt credential appended behind a genuine one would otherwise be relayed
    /// (#392). A slot with any consumed value is removed **entirely**:
    /// `HeaderMap::remove` clears every value for the name, so a genuine
    /// credential sharing the slot goes with it. That over-strip is deliberate
    /// and is asserted in the tests — the alternative, rebuilding the slot from
    /// its surviving values, adds a second place where "which values are
    /// shunt's" is decided, which is the drift this module exists to remove.
    ///
    /// Returns what was stripped — slot name and matched credential kind — so
    /// callers can log *why* without logging the token. `Vec::new()` does not
    /// allocate, so the common nothing-to-strip path stays free.
    pub(crate) fn strip_consumed_slots(
        &self,
        headers: &mut HeaderMap,
    ) -> Vec<(&'static str, ConsumedBy)> {
        let mut stripped = Vec::new();
        if let Some(reason) = authorization_consumed_by(
            headers,
            self.gateway_auth,
            self.static_auth,
            self.admin_credentials,
        ) {
            headers.remove(AUTHORIZATION);
            stripped.push((AUTHORIZATION, reason));
        }
        if let Some(reason) = headers.get_all(API_KEY).iter().find_map(|value| {
            consumed_by(
                value.as_bytes(),
                self.gateway_auth,
                self.static_auth,
                self.admin_credentials,
            )
        }) {
            headers.remove(API_KEY);
            stripped.push((API_KEY, reason));
        }
        stripped
    }
}

#[cfg(test)]
mod tests;
