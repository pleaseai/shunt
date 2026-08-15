//! `[[server.admin.write_keys]]` / `[[server.admin.read_keys]]` — the
//! per-credential admin key arrays and the resolved keyring the admin and
//! spend-limit surfaces authenticate against.
//!
//! `[server.admin]` historically carried a single uniform tier: the
//! `name:token` pairs in `tokens_env`/`tokens_file`, every one of which can
//! provision upstream accounts. The arrays add a per-credential `id` (what the
//! spend audit trail attributes to) and a `read` tier that can see the admin
//! surface and the spend-limit API without mutating either. The legacy token
//! pairs are the write tier — full access is read plus write.
//!
//! Array key material is a [`Secret`], so it is redacted in `Debug`/`Serialize`
//! and `[server.admin]` must stay behind its `Option` (see `secrets.rs` for why
//! a `Secret` must never be reachable from `Serialized::defaults`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{ConfigError, Secret};
use crate::auth::inbound::constant_time_eq;

/// Minimum length of an array key. Enforced on the arrays only: `tokens_env`
/// has no minimum today and adding one would reject existing deployments, so a
/// short legacy token warns instead (see [`warn_short_tokens`]).
const MIN_ADMIN_KEY_LENGTH: usize = 32;

/// Admin privilege level. `Read < Write`, so `write` implies `read`: a required
/// level is enforced by comparison, and a credential is resolved to the
/// *maximum* over every set that matched rather than to whichever set was
/// scanned last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAccess {
    Read,
    Write,
}

/// One `[[server.admin.write_keys]]` / `[[server.admin.read_keys]]` entry. The
/// `id` is safe to log and is what the audit trail records (`admin-key:<id>`);
/// the key never is.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminKey {
    pub id: String,
    pub key: Secret,
}

/// The credential a request presented: its resolved privilege plus the audit
/// actor string. Ordered by `access` first, so taking a maximum over several
/// matching sets picks the highest privilege.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdminCredential {
    pub access: AdminAccess,
    /// `admin-token:<name>` for a `tokens_env`/`tokens_file` pair,
    /// `admin-key:<id>` for an array entry.
    pub actor: String,
}

/// One resolved credential in the keyring: what it may do, the audit actor it
/// is recorded as, and the key material itself.
#[derive(Debug, Clone)]
struct AdminKeyEntry {
    access: AdminAccess,
    actor: String,
    key: Secret,
}

/// Every credential `[server.admin]` resolved — the `tokens_env`/`tokens_file`
/// pairs *and* both key arrays — in one place, consulted on every admin and
/// spend-limit request and by the outbound strip predicate
/// (`crate::auth::inbound::consumed_by`). Built by `AdminConfig::resolve` once
/// the arrays have passed [`validate_key_arrays`] and [`check_key_uniqueness`],
/// so no entry here can be blank, too short, or ambiguous with another set.
///
/// It is the single source of truth for "is this value an admin credential":
/// the accept path (`AdminAuth::authenticate_credential`) and the strip path
/// must agree, or a credential shunt accepts in a slot would be forwarded
/// upstream from that same slot.
#[derive(Debug, Clone, Default)]
pub struct AdminKeyring {
    entries: Vec<AdminKeyEntry>,
}

impl AdminKeyring {
    /// `tokens` are the legacy `name:token` pairs: the write tier, recorded as
    /// `admin-token:<name>`. Array entries are recorded as `admin-key:<id>`.
    pub(crate) fn new(
        tokens: &[(String, String)],
        write_keys: &[AdminKey],
        read_keys: &[AdminKey],
    ) -> Self {
        let token_entries = tokens.iter().map(|(name, token)| AdminKeyEntry {
            access: AdminAccess::Write,
            actor: format!("admin-token:{name}"),
            key: Secret::from(token.as_str()),
        });
        let array_entries = write_keys
            .iter()
            .map(|entry| (AdminAccess::Write, entry))
            .chain(read_keys.iter().map(|entry| (AdminAccess::Read, entry)))
            .map(|(access, entry)| AdminKeyEntry {
                access,
                actor: format!("admin-key:{}", entry.id),
                key: entry.key.clone(),
            });
        Self {
            entries: token_entries.chain(array_entries).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The highest access level `presented` matches, with the audit actor it
    /// matched as. Every entry is compared with no early exit, so timing does
    /// not reveal which credential matched, and the result is an explicit
    /// maximum over the whole keyring: reordering the sets cannot change a
    /// credential's privilege. Values are unique across every set
    /// (`check_key_uniqueness`), so at most one entry can match and the
    /// tie-break on the actor string is unreachable.
    pub fn lookup(&self, presented: &[u8]) -> Option<(AdminAccess, &str)> {
        let mut matched: Option<(AdminAccess, &str)> = None;
        for entry in &self.entries {
            if constant_time_eq(presented, entry.key.expose().as_bytes()) {
                let candidate = (entry.access, entry.actor.as_str());
                matched = Some(match matched {
                    Some(existing) => std::cmp::max(existing, candidate),
                    None => candidate,
                });
            }
        }
        matched
    }

    /// Whether `presented` is an admin credential of any tier. Used by the
    /// outbound strip predicate, which only needs the yes/no answer — a read
    /// key must be stripped just as a write key is.
    pub fn contains(&self, presented: &[u8]) -> bool {
        self.lookup(presented).is_some()
    }
}

/// Shape checks that need nothing but the config file: every array entry
/// carries a non-blank `id` and a key of at least [`MIN_ADMIN_KEY_LENGTH`]
/// bytes. Id and value *uniqueness* is checked by [`check_key_uniqueness`]
/// instead, which also sees the env-resolved `tokens_env` pairs.
pub(crate) fn validate_key_arrays(
    write_keys: &[AdminKey],
    read_keys: &[AdminKey],
) -> Result<(), ConfigError> {
    for (field, keys) in [("write_keys", write_keys), ("read_keys", read_keys)] {
        for (index, entry) in keys.iter().enumerate() {
            if entry.id.trim().is_empty() {
                return Err(ConfigError::BlankAdminKeyId { field, index });
            }
            if entry.key.expose().len() < MIN_ADMIN_KEY_LENGTH {
                return Err(ConfigError::ShortAdminKey {
                    field,
                    id: entry.id.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Ids and key values must both be unique across all three credential sets:
/// a duplicated id makes audit attribution ambiguous, and a duplicated value
/// makes privilege ambiguous. Neither error echoes key material.
pub(crate) fn check_key_uniqueness(
    tokens: &[(String, String)],
    write_keys: &[AdminKey],
    read_keys: &[AdminKey],
) -> Result<(), ConfigError> {
    let entries = tokens
        .iter()
        .map(|(name, token)| (name.as_str(), token.as_str()))
        .chain(
            write_keys
                .iter()
                .chain(read_keys)
                .map(|entry| (entry.id.as_str(), entry.key.expose())),
        );
    let mut ids = HashSet::new();
    let mut values = HashMap::<&str, &str>::new();
    for (id, value) in entries {
        if !ids.insert(id) {
            return Err(ConfigError::DuplicateAdminKeyId { id: id.to_string() });
        }
        if let Some(first_id) = values.insert(value, id) {
            return Err(ConfigError::DuplicateAdminKeyValue {
                first_id: first_id.to_string(),
                second_id: id.to_string(),
            });
        }
    }
    Ok(())
}

/// A `tokens_env`/`tokens_file` pair shorter than the array minimum warns
/// rather than failing: those tokens predate any length rule and enforcing one
/// would refuse to start deployments that work today.
pub(crate) fn warn_short_tokens(tokens: &[(String, String)], source: &str) {
    for (name, token) in tokens {
        if token.len() < MIN_ADMIN_KEY_LENGTH {
            tracing::warn!(
                name = %name,
                source = %source,
                "[server.admin] token is shorter than {MIN_ADMIN_KEY_LENGTH} characters; \
                 prefer [[server.admin.write_keys]] or a longer token"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, value: &str) -> AdminKey {
        AdminKey {
            id: id.to_string(),
            key: Secret::from(value),
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn lookup_resolves_the_maximum_access_regardless_of_set_order() {
        // The same id/value registered as both tiers cannot occur in a real
        // config (`check_key_uniqueness` rejects it), but building the keyring
        // both ways directly is what proves the resolution is a maximum rather
        // than "whichever set was scanned last".
        let write = vec![key("dual", A)];
        let read = vec![key("dual", A)];
        assert_eq!(
            AdminKeyring::new(&[], &write, &read).lookup(A.as_bytes()),
            Some((AdminAccess::Write, "admin-key:dual"))
        );
        // Feeding the sets in the other order must not change the answer.
        assert_eq!(
            AdminKeyring::new(&[], &read, &write).lookup(A.as_bytes()),
            Some((AdminAccess::Write, "admin-key:dual"))
        );
    }

    #[test]
    fn lookup_returns_the_matching_tier_and_actor() {
        let tokens = vec![("ops".to_string(), "ops-token-value".to_string())];
        let keyring = AdminKeyring::new(&tokens, &[key("terraform", A)], &[key("reporting", B)]);
        assert_eq!(
            keyring.lookup(A.as_bytes()),
            Some((AdminAccess::Write, "admin-key:terraform"))
        );
        assert_eq!(
            keyring.lookup(B.as_bytes()),
            Some((AdminAccess::Read, "admin-key:reporting"))
        );
        // A legacy `tokens_env` pair is the write tier, attributed by name.
        assert_eq!(
            keyring.lookup(b"ops-token-value"),
            Some((AdminAccess::Write, "admin-token:ops"))
        );
        assert_eq!(keyring.lookup(b"nonsense"), None);
        // `contains` is the strip predicate's view of the same table: every
        // tier counts, the read tier included.
        assert!(keyring.contains(A.as_bytes()));
        assert!(keyring.contains(B.as_bytes()));
        assert!(keyring.contains(b"ops-token-value"));
        assert!(!keyring.contains(b"a-genuine-upstream-key"));
    }

    #[test]
    fn validate_key_arrays_rejects_blank_ids_and_short_keys() {
        assert!(matches!(
            validate_key_arrays(&[key("  ", A)], &[]),
            Err(ConfigError::BlankAdminKeyId {
                field: "write_keys",
                index: 0
            })
        ));
        assert!(matches!(
            validate_key_arrays(&[], &[key("reporting", "short")]),
            Err(ConfigError::ShortAdminKey {
                field: "read_keys",
                ..
            })
        ));
        validate_key_arrays(&[key("terraform", A)], &[key("reporting", B)])
            .expect("distinct, long-enough keys validate");
    }

    #[test]
    fn key_uniqueness_spans_tokens_and_both_arrays() {
        let tokens = vec![("ops".to_string(), "ops-token-value".to_string())];
        assert!(matches!(
            check_key_uniqueness(&tokens, &[key("ops", A)], &[]),
            Err(ConfigError::DuplicateAdminKeyId { id }) if id == "ops"
        ));
        let error = check_key_uniqueness(&tokens, &[key("terraform", A)], &[key("reporting", A)])
            .expect_err("the same value in two arrays is ambiguous");
        assert!(matches!(
            error,
            ConfigError::DuplicateAdminKeyValue { ref first_id, ref second_id }
                if first_id == "terraform" && second_id == "reporting"
        ));
        assert!(!error.to_string().contains(A));
        // A token value reused as an array key is caught the same way.
        assert!(matches!(
            check_key_uniqueness(&tokens, &[key("terraform", "ops-token-value")], &[]),
            Err(ConfigError::DuplicateAdminKeyValue { .. })
        ));
        check_key_uniqueness(&tokens, &[key("terraform", A)], &[key("reporting", B)])
            .expect("distinct ids and values across all three sets");
    }
}
