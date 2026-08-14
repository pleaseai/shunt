//! Antigravity subscription OAuth: credential store, login, and the client
//! version fingerprint the backend is addressed with.

use std::{env, path::PathBuf};

pub mod auth;
pub mod login;
pub mod version;

/// shunt-owned Antigravity credential file: `$SHUNT_ANTIGRAVITY_AUTH_FILE`, else
/// `~/.shunt/antigravity-auth.json`. Written by `shunt login antigravity` and
/// refreshed by shunt alone — unlike the Gemini path, no other tool owns it.
pub fn default_antigravity_auth_path() -> PathBuf {
    env::var_os("SHUNT_ANTIGRAVITY_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            // `HOME` is unset on Windows; fall back to `USERPROFILE` so the
            // credential lands in the user's home rather than a
            // working-directory-relative path.
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
                .map(PathBuf::from)
                .map(|home| home.join(".shunt").join("antigravity-auth.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".shunt/antigravity-auth.json"))
}
