//! Shunt-owned ChatGPT-plan account files.
//!
//! Each account is a named file at `~/.shunt/accounts/chatgpt-plan/<name>.json`
//! (or `$SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR/<name>.json`).

use std::{io, path::PathBuf};

use crate::auth::shared;
use crate::config::AccountConfig;

pub use crate::auth::shared::validate_account_name;

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", "chatgpt-plan")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. No verified
/// upstream account identifier is read, so every entry falls back to its own
/// file name as its pool identity.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), |_path| None)
}
