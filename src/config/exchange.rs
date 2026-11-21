use serde::{Deserialize, Serialize};

use super::{session::deserialize_one_or_many_secrets, ConfigError, Secret};

/// `[server.auth.exchange]` — lets a workload (for example a GitHub Actions
/// run) trade a verified external JWT for a short-lived shunt-issued token.
/// Entries opt in with `exchange = true` on `[[server.auth.jwt]]`.
///
/// Holds signing secrets, so it lives behind an `Option` on
/// `InboundAuthConfig` (see the `Secret` INVARIANT in `secrets.rs`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeConfig {
    /// One or more HS256 signing secrets, each at least 32 bytes. A bare
    /// string is a single secret; an array supports rotation (the first signs,
    /// every entry verifies).
    #[serde(deserialize_with = "deserialize_one_or_many_secrets")]
    pub signing_secret: Vec<Secret>,
    /// Lifetime of an issued token. Defaults to 3600; must be non-zero.
    #[serde(default = "default_max_ttl_seconds")]
    pub max_ttl_seconds: u64,
}

fn default_max_ttl_seconds() -> u64 {
    3600
}

impl ExchangeConfig {
    /// Validate and resolve into signing state. Empty list, a secret under 32
    /// bytes (index named), and a zero lifetime are errors.
    pub(crate) fn resolve(&self) -> Result<crate::auth::exchange::ExchangeSettings, ConfigError> {
        if self.signing_secret.is_empty() {
            return Err(ConfigError::EmptyExchangeSigningSecret);
        }
        if self.max_ttl_seconds == 0 {
            return Err(ConfigError::InvalidExchangeMaxTtl);
        }
        let secrets = self
            .signing_secret
            .iter()
            .enumerate()
            .map(|(index, secret)| {
                if secret.expose().len() < 32 {
                    Err(ConfigError::InvalidExchangeSigningSecret { index })
                } else {
                    Ok(secret.expose().as_bytes().to_vec())
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::auth::exchange::ExchangeSettings::new(
            secrets,
            self.max_ttl_seconds,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn parse(text: &str) -> ExchangeConfig {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn accepts_bare_and_array_secrets_with_default_ttl() {
        let bare = parse(&format!(r#"signing_secret = "{SECRET}""#));
        let settings = bare.resolve().unwrap();
        assert_eq!(settings.max_ttl_seconds(), 3600);
        assert_eq!(settings.verifying_secrets().len(), 1);
        let many = parse(&format!(
            r#"signing_secret = ["{SECRET}", "fedcba9876543210fedcba9876543210"]
max_ttl_seconds = 600"#
        ));
        let settings = many.resolve().unwrap();
        assert_eq!(settings.max_ttl_seconds(), 600);
        assert_eq!(settings.signing_secret(), SECRET.as_bytes());
        assert_eq!(settings.verifying_secrets().len(), 2);
    }

    #[test]
    fn rejects_empty_secret_list() {
        let error = parse("signing_secret = []").resolve().unwrap_err();
        assert!(matches!(error, ConfigError::EmptyExchangeSigningSecret));
    }

    #[test]
    fn rejects_short_secret_naming_its_index() {
        let error = parse(&format!(r#"signing_secret = ["{SECRET}", "short"]"#))
            .resolve()
            .unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidExchangeSigningSecret { index: 1 }
        ));
    }

    #[test]
    fn rejects_zero_ttl() {
        let error = parse(&format!(
            r#"signing_secret = "{SECRET}"
max_ttl_seconds = 0"#
        ))
        .resolve()
        .unwrap_err();
        assert!(matches!(error, ConfigError::InvalidExchangeMaxTtl));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<ExchangeConfig>(&format!(
            "signing_secret = \"{SECRET}\"\nttl = 5"
        ))
        .is_err());
    }

    #[test]
    fn debug_and_serialize_never_print_the_secret() {
        let config = parse(&format!(r#"signing_secret = "{SECRET}""#));
        assert!(!format!("{config:?}").contains(SECRET));
        assert!(!toml::to_string(&config).unwrap().contains(SECRET));
        let settings = config.resolve().unwrap();
        assert!(!format!("{settings:?}").contains(SECRET));
    }
}
