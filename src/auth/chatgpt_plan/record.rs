//! The on-disk shape of one chatgpt-plan account file.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Current `version` field of an account file.
pub const RECORD_VERSION: u32 = 1;

/// One account file, `<accounts dir>/<name>.json`.
///
/// A signed-out account is exactly `{version, client_id}`: the registration
/// survives so a re-sign-in reuses the issued `client_id`, and every other
/// field is omitted rather than written empty.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub version: u32,
    /// The issued `oaiapp_...` client id.
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenBlock>,
}

/// Credential material of a signed-in account. Times are epoch seconds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBlock {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest_refresh_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<u64>,
}

impl AccountRecord {
    /// The signed-out form of an account: only the registration is kept.
    pub fn signed_out(client_id: impl Into<String>) -> Self {
        Self {
            version: RECORD_VERSION,
            client_id: client_id.into(),
            sub: None,
            email: None,
            scopes: Vec::new(),
            tokens: None,
        }
    }

    /// This account reduced to its signed-out form (keeps only `client_id`).
    pub fn into_signed_out(self) -> Self {
        Self::signed_out(self.client_id)
    }

    pub fn is_signed_in(&self) -> bool {
        self.tokens.is_some()
    }
}

// Hand-written so token values can never reach a log line (NFR-2).
impl fmt::Debug for TokenBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBlock")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .field("earliest_refresh_at", &self.earliest_refresh_at)
            .field("last_refresh", &self.last_refresh)
            .finish()
    }
}

impl fmt::Debug for AccountRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountRecord")
            .field("version", &self.version)
            .field("client_id", &self.client_id)
            .field("sub", &self.sub)
            .field("email", &self.email)
            .field("scopes", &self.scopes)
            .field("tokens", &self.tokens)
            .finish()
    }
}
