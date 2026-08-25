//! `shunt gateway` — logging *in to* a self-hosted shunt deployment.
//!
//! The mirror image of [`crate::gateway`], which is the server side: that
//! module answers the OAuth device flow, this one drives it as a client. The
//! result is a single session file ([`store`]) holding one deployment's token
//! pair, refreshed on demand by [`auth`] so `shunt gateway token` can be wired
//! up as a Claude Code `apiKeyHelper`.
//!
//! Distinct from `shunt login <provider>` / `shunt token`, which authenticate
//! shunt against an *upstream* subscription. Nothing here touches those
//! credentials.

pub mod auth;
pub mod launch;
pub mod login;
pub mod store;
