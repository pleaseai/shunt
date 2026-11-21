//! Workload token exchange (`[server.auth.exchange]`): shunt-issued,
//! short-lived tokens traded for a verified external assertion.
//!
//! This module currently holds the resolved configuration and the issuer
//! constant; minting and verification live beside them as the feature lands.

/// The `iss` of every exchange token. A `[[server.auth.jwt]]` entry may not
/// claim it (config validation), so the exchange verifier and an external
/// issuer can never be confused for each other.
pub(crate) const EXCHANGE_ISSUER: &str = "urn:shunt:workload-exchange";

/// Resolved `[server.auth.exchange]`: the ordered signing secrets (index 0
/// signs, every entry verifies) and the issued-token lifetime.
#[derive(Clone)]
pub struct ExchangeSettings {
    secrets: Vec<Vec<u8>>,
    max_ttl_seconds: u64,
}

impl ExchangeSettings {
    pub(crate) fn new(secrets: Vec<Vec<u8>>, max_ttl_seconds: u64) -> Self {
        assert!(
            !secrets.is_empty(),
            "ExchangeSettings requires at least one signing secret"
        );
        Self {
            secrets,
            max_ttl_seconds,
        }
    }

    /// The secret new tokens are signed with. Read by the minting code that
    /// the exchange endpoint adds; only config tests use it until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn signing_secret(&self) -> &[u8] {
        &self.secrets[0]
    }

    /// Every secret a presented token may verify under.
    pub(crate) fn verifying_secrets(&self) -> &[Vec<u8>] {
        &self.secrets
    }

    pub fn max_ttl_seconds(&self) -> u64 {
        self.max_ttl_seconds
    }
}

impl std::fmt::Debug for ExchangeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangeSettings")
            .field(
                "secrets",
                &format_args!("[redacted; {}]", self.secrets.len()),
            )
            .field("max_ttl_seconds", &self.max_ttl_seconds)
            .finish()
    }
}
