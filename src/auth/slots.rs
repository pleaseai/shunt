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
//! | `x-shunt-token`, `x-shunt-admin-token`, `x-shunt-inbound-client`, plus whatever `[server.auth] header` / `[server.admin] header` name | reserved by shunt | **by name** ([`ShuntCredentials::strip_reserved_slots`]) |
//!
//! ## Accept sites
//!
//! - [`crate::auth::inbound::InboundAuth::authenticate`] — the configured
//!   `[server.auth] header`, raw.
//! - [`crate::auth::inbound::InboundAuth::authenticate_bearer`] — that header
//!   raw, plus the `Authorization: Bearer` payload.
//! - [`crate::auth::inbound::InboundAuth::authenticate_client`] — that header
//!   raw, plus the `Authorization: Bearer` payload, plus `x-api-key` raw.
//! - [`crate::gateway::GatewayAuth::authenticate_bearer`] — the
//!   `Authorization: Bearer` payload; `authenticate_token` the bare value.
//! - [`crate::admin::AdminAuth::authenticate_credential`] — the configured
//!   `[server.admin] header` raw **and** `x-api-key` raw, over every
//!   `[server.admin]` credential (`write_keys`, `read_keys`, and the legacy
//!   `tokens_env`/`tokens_file` pairs alike).
//!
//! Their callers — `discovery`, `usage`, `oauth_usage`, `codex_analytics`,
//! `codex_endpoint`, `proxy::failover`, `gateway::telemetry_ingest`,
//! `gateway::managed`, `gateway::spend::api`, `admin` — add no slot of their
//! own; they only choose which of the predicates above to run.
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
//! `tests::every_bulk_header_forward_is_a_registered_site` walks `src/**/*.rs`
//! for a bulk application of a header map to an outbound request and asserts
//! the file set matches a hard-coded allowlist, so a new relay path has to be
//! classified rather than merely compile. Its residual hole is stated with the
//! allowlist: a site that appends headers one at a time in a loop is not
//! caught (`discovery/upstream.rs` itself does exactly that).

use axum::http::{HeaderMap, HeaderName};

use crate::{admin::AdminAuth, config::AdminKeyring, gateway::GatewayAuth, server::AppState};

use super::inbound::{authorization_consumed_by, consumed_by, ConsumedBy, InboundAuth};

/// The two slots shunt shares with the caller's own upstream credential.
/// Values here are stripped **by value**, never by slot name: an
/// `apiKeyHelper` fills both with the same value, so either can hold a shunt
/// credential beside a genuine upstream credential in the other.
pub(crate) const SHARED_SLOTS: [&str; 2] = ["authorization", "x-api-key"];

/// Header names shunt reserves whatever `[server.auth]`/`[server.admin]` are
/// configured to. Removed unconditionally — even on an ungated endpoint — so
/// the documented guarantee holds without depending on config: none of these
/// is ever a legitimate upstream header, so removing a name a client sent
/// cannot break a legitimate relay.
pub(crate) const RESERVED_SLOTS: [&str; 3] = [
    "x-shunt-token",
    "x-shunt-admin-token",
    "x-shunt-inbound-client",
];

/// The two [`SHARED_SLOTS`] under names the strip code can use, so the code and
/// the list cannot drift apart.
const AUTHORIZATION: &str = SHARED_SLOTS[0];
const API_KEY: &str = SHARED_SLOTS[1];

/// Everything a forward site needs to recognize one of shunt's own inbound
/// credentials: the three credential tables to check values against, and the
/// two configurable header names to clear by name.
///
/// `Copy`, so a site can take it by value and still hand it to a helper.
#[derive(Clone, Copy, Default)]
pub(crate) struct ShuntCredentials<'a> {
    pub(crate) gateway_auth: Option<&'a GatewayAuth>,
    pub(crate) static_auth: Option<&'a InboundAuth>,
    pub(crate) admin_credentials: Option<&'a AdminKeyring>,
    /// `[server.auth] header`, when configured.
    pub(crate) static_header: Option<&'a HeaderName>,
    /// `[server.admin] header`, when configured.
    pub(crate) admin_header: Option<&'a HeaderName>,
}

impl<'a> ShuntCredentials<'a> {
    /// The single wiring point from request state. Forward sites call this
    /// rather than reading `AppState` fields themselves, so adding a credential
    /// kind is one edit here instead of an edit per site that some future
    /// change can forget.
    pub(crate) fn from_state(state: &'a AppState) -> Self {
        Self {
            gateway_auth: state.gateway_auth.as_deref(),
            static_auth: state.inbound_auth.as_deref(),
            admin_credentials: state.admin_auth.as_deref().map(AdminAuth::credentials),
            static_header: state.inbound_auth.as_deref().map(InboundAuth::header),
            admin_header: state.admin_auth.as_deref().map(AdminAuth::header),
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
        if let Some(reason) = headers.get(API_KEY).and_then(|value| {
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
