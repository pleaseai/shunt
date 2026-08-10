use std::{path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};

/// Nineteen decimal digits always fit in the unsigned 64-bit arithmetic used
/// by the next enforcement stage.
pub(crate) const MAX_AMOUNT_LENGTH: usize = 19;
/// User identifiers are opaque, but bounding them limits persisted snapshots.
pub(crate) const MAX_USER_ID_LENGTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationError {
    Amount,
    Currency,
    ObjectType,
    UserId,
}

impl ValidationError {
    pub(crate) fn field(self) -> &'static str {
        match self {
            Self::Amount => "amount",
            Self::Currency => "currency",
            Self::ObjectType => "type",
            Self::UserId => "scope.user_id",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Scope {
    User { user_id: String },
    Organization,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Daily,
    Weekly,
    #[default]
    Monthly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendLimit {
    pub id: String,
    pub amount: Option<String>,
    pub created_at: String,
    pub currency: String,
    pub period: Period,
    pub scope: Scope,
    #[serde(rename = "type")]
    pub object_type: String,
    pub updated_at: String,
}

pub(crate) fn validate_amount(amount: Option<&str>) -> Result<(), ValidationError> {
    if amount.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_AMOUNT_LENGTH
            || !value.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return Err(ValidationError::Amount);
    }
    Ok(())
}

pub(crate) fn validate_scope(scope: &Scope) -> Result<(), ValidationError> {
    if let Scope::User { user_id } = scope {
        if user_id.is_empty() || user_id.len() > MAX_USER_ID_LENGTH {
            return Err(ValidationError::UserId);
        }
    }
    Ok(())
}

pub(crate) fn validate_limit(limit: &SpendLimit) -> Result<(), ValidationError> {
    validate_amount(limit.amount.as_deref())?;
    if limit.currency != "USD" {
        return Err(ValidationError::Currency);
    }
    if limit.object_type != "spend_limit" {
        return Err(ValidationError::ObjectType);
    }
    validate_scope(&limit.scope)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub id: u64,
    pub created_at: String,
    pub actor: String,
    pub before: Option<SpendLimit>,
    pub after: Option<SpendLimit>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct SpendState {
    pub limits: Vec<SpendLimit>,
    pub audit: Vec<AuditRecord>,
    pub next_audit_id: u64,
}

pub struct SpendStore {
    state: Mutex<SpendState>,
    mutation_gate: tokio::sync::Mutex<()>,
    state_path: Option<PathBuf>,
}

impl SpendStore {
    pub fn new(state_path: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(SpendState {
                next_audit_id: 1,
                ..SpendState::default()
            }),
            mutation_gate: tokio::sync::Mutex::new(()),
            state_path,
        }
    }

    pub(crate) async fn mutation_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutation_gate.lock().await
    }

    pub fn get(&self, id: &str) -> Option<SpendLimit> {
        self.state
            .lock()
            .expect("gateway spend-limit lock poisoned")
            .limits
            .iter()
            .find(|limit| limit.id == id)
            .cloned()
    }

    pub fn list(&self) -> Vec<SpendLimit> {
        self.state
            .lock()
            .expect("gateway spend-limit lock poisoned")
            .limits
            .clone()
    }

    pub(crate) fn upsert_state(
        mut state: SpendState,
        scope: Scope,
        period: Period,
        amount: Option<String>,
        actor: &str,
        now: String,
    ) -> (SpendState, SpendLimit) {
        let position = state
            .limits
            .iter()
            .position(|limit| limit.scope == scope && limit.period == period);
        if let Some(index) = position {
            if state.limits[index].amount == amount {
                let limit = state.limits[index].clone();
                return (state, limit);
            }
        }
        let before = position.map(|index| state.limits[index].clone());
        let limit = match position {
            Some(index) => {
                let limit = &mut state.limits[index];
                limit.amount = amount;
                limit.updated_at = now.clone();
                limit.clone()
            }
            None => {
                let limit = SpendLimit {
                    id: format!("spl_{}", crate::admin::session::random_id()),
                    amount,
                    created_at: now.clone(),
                    currency: "USD".to_string(),
                    period,
                    scope,
                    object_type: "spend_limit".to_string(),
                    updated_at: now.clone(),
                };
                state.limits.push(limit.clone());
                limit
            }
        };
        append_audit(&mut state, actor, now, before, Some(limit.clone()));
        (state, limit)
    }

    #[cfg(test)]
    pub fn upsert(
        &self,
        scope: Scope,
        period: Period,
        amount: Option<String>,
        actor: &str,
        now: String,
    ) -> SpendLimit {
        let mut current = self
            .state
            .lock()
            .expect("gateway spend-limit lock poisoned");
        let (state, limit) = Self::upsert_state(current.clone(), scope, period, amount, actor, now);
        *current = state;
        limit
    }

    pub(crate) fn delete_state(
        mut state: SpendState,
        id: &str,
        actor: &str,
        now: String,
    ) -> Option<(SpendState, SpendLimit)> {
        let position = state.limits.iter().position(|limit| limit.id == id)?;
        let before = state.limits.remove(position);
        append_audit(&mut state, actor, now, Some(before.clone()), None);
        Some((state, before))
    }

    #[cfg(test)]
    pub fn delete(&self, id: &str, actor: &str, now: String) -> Option<SpendLimit> {
        let mut current = self
            .state
            .lock()
            .expect("gateway spend-limit lock poisoned");
        let (state, deleted) = Self::delete_state(current.clone(), id, actor, now)?;
        *current = state;
        Some(deleted)
    }

    pub(crate) fn export(&self) -> SpendState {
        self.state
            .lock()
            .expect("gateway spend-limit lock poisoned")
            .clone()
    }

    pub fn state_path(&self) -> Option<&std::path::Path> {
        self.state_path.as_deref()
    }

    pub(crate) fn replace(&self, state: SpendState) -> SpendState {
        std::mem::replace(
            &mut *self
                .state
                .lock()
                .expect("gateway spend-limit lock poisoned"),
            state,
        )
    }
}

impl Default for SpendStore {
    fn default() -> Self {
        Self::new(None)
    }
}

fn append_audit(
    state: &mut SpendState,
    actor: &str,
    created_at: String,
    before: Option<SpendLimit>,
    after: Option<SpendLimit>,
) {
    let id = state.next_audit_id;
    state.next_audit_id = state.next_audit_id.saturating_add(1);
    state.audit.push(AuditRecord {
        id,
        created_at,
        actor: actor.to_string(),
        before,
        after,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_records_have_monotonic_ids_actor_and_snapshots() {
        let store = SpendStore::new(None);
        let first = store.upsert(
            Scope::Organization,
            Period::Monthly,
            Some("100".into()),
            "admin-key:writer",
            "2026-08-09T00:00:00.000Z".into(),
        );
        let second = store.upsert(
            Scope::Organization,
            Period::Monthly,
            Some("200".into()),
            "admin-key:writer",
            "2026-08-09T00:00:01.000Z".into(),
        );
        store.delete(
            &second.id,
            "admin-key:writer",
            "2026-08-09T00:00:02.000Z".into(),
        );

        let audit = store.export().audit;
        assert_eq!(
            audit.iter().map(|record| record.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(audit
            .iter()
            .all(|record| record.actor == "admin-key:writer"));
        assert_eq!(audit[0].before, None);
        assert_eq!(audit[0].after, Some(first.clone()));
        assert_eq!(audit[1].before, Some(first));
        assert_eq!(audit[1].after, Some(second.clone()));
        assert_eq!(audit[2].before, Some(second));
        assert_eq!(audit[2].after, None);
    }
}
