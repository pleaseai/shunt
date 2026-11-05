//! One pooled request's session-binding bookkeeping (ADR-0008 §2).
//!
//! Selection hands an adapter a [`BindingTicket`]; the adapter walks its
//! failover order and, at the one point an account serves a success, commits
//! the ticket with that account. [`SessionCommit`] carries the ticket through
//! the loop, notes whether the bound account was attempted (so a later serve
//! elsewhere counts as a `failover` move), and records the switch metric for a
//! move. It holds no lock: the commit is synchronous and takes the pool's
//! binding lock only for its own duration.
//!
//! Every method takes `&self`, so a stream-driven account loop (the Responses
//! pool's `stream::unfold`) can share one behind an `Arc` across its steps.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::StatusCode;

use crate::{
    accounts::{AccountPool, BindingCommit, BindingTicket},
    config::{AccountConfig, AuthMode},
    routing::Route,
};

pub(crate) struct SessionCommit {
    ticket: Option<BindingTicket>,
    /// The `thinking_bound` label of a switch this request records.
    thinking_bound: bool,
    /// The bound account was attempted on this request. Set when an attempt
    /// on it starts; any later serve by another account means it failed over.
    bound_attempted: AtomicBool,
}

impl SessionCommit {
    /// `ticket` is the session's binding ticket from selection, `None` for a
    /// sessionless request (which commits nothing, FR-15).
    fn new(ticket: Option<BindingTicket>, thinking_bound: bool) -> Self {
        Self {
            ticket,
            thinking_bound,
            bound_attempted: AtomicBool::new(false),
        }
    }

    /// [`new`](Self::new) with the `thinking_bound` label derived from the
    /// request's route and its provider's `auth`.
    pub(crate) fn for_route(ticket: Option<BindingTicket>, route: &Route, auth: AuthMode) -> Self {
        Self::new(
            ticket,
            crate::accounts::thinking_bound(&route.adapter, auth, &route.upstream_model),
        )
    }

    /// Call once an attempt on `account` begins, after storm-control
    /// admission: an admission skip is not an attempt, but a failed credential
    /// resolve or refresh, a transport error and a rotating status all are.
    pub(crate) fn attempting(&self, account: &AccountConfig) {
        if !self.bound_attempted.load(Ordering::Relaxed)
            && self
                .ticket
                .as_ref()
                .is_some_and(|ticket| ticket.is_bound_to(account))
        {
            self.bound_attempted.store(true, Ordering::Relaxed);
        }
    }

    /// Commit the binding when `served` relayed `status`. Only a success is a
    /// serve point; a relayed 4xx/5xx commits nothing. A move records one
    /// `shunt.pool.session_account_switches` sample.
    pub(crate) fn served(
        &self,
        pool: &AccountPool,
        provider: &str,
        served: &AccountConfig,
        status: StatusCode,
    ) {
        if !status.is_success() {
            return;
        }
        let Some(ticket) = self.ticket.as_ref() else {
            return;
        };
        // `bound_attempted` while the bound account itself served is a touch:
        // the commit compares the served account with the binding first.
        let commit = pool.commit_session_binding(
            ticket,
            served,
            self.bound_attempted.load(Ordering::Relaxed),
        );
        if let BindingCommit::Moved(reason) = commit {
            crate::metrics::record_session_account_switch(
                provider,
                reason.as_str(),
                self.thinking_bound,
            );
        }
    }
}
