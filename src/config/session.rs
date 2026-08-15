use serde::{Deserialize, Serialize};

use super::Secret;

/// `[server.gateway.session]` — mirrors the upstream Claude apps gateway
/// `session:` config
/// (<https://code.claude.com/docs/en/claude-apps-gateway-config#session>).
/// Supersedes `[server.gateway]`'s older `jwt_secret_env` and
/// `token_ttl_seconds` keys: `jwt_secret` lives directly in the resolved
/// config (behind `${VAR}`/`${file:}` references, or literally with a boot
/// warning) instead of naming a separately-set environment variable, and
/// giving it more than one entry supports rotation — the first entry signs
/// new tokens, every entry verifies a presented one. `GatewayConfig::resolve`
/// rejects setting both a legacy key and its `session` replacement.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewaySessionConfig {
    /// One or more HS256 signing secrets, each at least 32 bytes. Accepts
    /// either a bare string (single secret) or an array (rotation).
    #[serde(deserialize_with = "deserialize_one_or_many_secrets")]
    pub jwt_secret: Vec<Secret>,
    /// Access-token lifetime in hours. Defaults to 1 hour (matching the
    /// legacy `token_ttl_seconds` default of 3600) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u64>,
}

fn deserialize_one_or_many_secrets<'de, D>(deserializer: D) -> Result<Vec<Secret>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Secret),
        Many(Vec<Secret>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(secret) => vec![secret],
        OneOrMany::Many(secrets) => secrets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_secret_accepts_a_bare_string() {
        let session: GatewaySessionConfig =
            toml::from_str(r#"jwt_secret = "0123456789abcdef0123456789abcdef""#).unwrap();
        assert_eq!(session.jwt_secret.len(), 1);
        assert_eq!(
            session.jwt_secret[0].expose(),
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(session.ttl_hours, None);
    }

    #[test]
    fn jwt_secret_accepts_an_array() {
        let session: GatewaySessionConfig = toml::from_str(
            r#"jwt_secret = ["0123456789abcdef0123456789abcdef", "fedcba9876543210fedcba9876543210"]
ttl_hours = 2"#,
        )
        .unwrap();
        assert_eq!(session.jwt_secret.len(), 2);
        assert_eq!(
            session.jwt_secret[0].expose(),
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            session.jwt_secret[1].expose(),
            "fedcba9876543210fedcba9876543210"
        );
        assert_eq!(session.ttl_hours, Some(2));
    }

    #[test]
    fn jwt_secret_is_required() {
        let err = toml::from_str::<GatewaySessionConfig>("ttl_hours = 2").unwrap_err();
        assert!(err.to_string().contains("jwt_secret"));
    }
}
