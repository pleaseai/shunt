use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use super::ConfigError;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccessControlConfig {
    #[serde(default)]
    pub allow_cidrs: Vec<String>,
    #[serde(default)]
    pub deny_cidrs: Vec<String>,
    /// Honor `X-Forwarded-For` and `X-Real-IP`. Enable this only behind a
    /// trusted proxy that replaces client-supplied forwarding headers.
    #[serde(default)]
    pub trust_forwarded_for: bool,
    #[serde(skip)]
    parsed_allow_cidrs: Vec<IpNet>,
    #[serde(skip)]
    parsed_deny_cidrs: Vec<IpNet>,
}

impl AccessControlConfig {
    /// Parse the configured CIDRs into the runtime lists used by [`Self::enabled`]
    /// and [`Self::allows`]. Callers must validate before constructing the HTTP
    /// tuning layer because the parsed lists are skipped during deserialization.
    pub(crate) fn validate(&mut self) -> Result<(), ConfigError> {
        self.parsed_allow_cidrs = parse_cidrs("allow_cidrs", &self.allow_cidrs)?;
        self.parsed_deny_cidrs = parse_cidrs("deny_cidrs", &self.deny_cidrs)?;
        Ok(())
    }

    /// Reports whether a policy contains any configured CIDRs. Using the raw
    /// lists here makes an accidentally unvalidated value active rather than
    /// silently disabling access control.
    pub(crate) fn enabled(&self) -> bool {
        !self.allow_cidrs.is_empty() || !self.deny_cidrs.is_empty()
    }

    pub(crate) fn allows(&self, address: Option<std::net::IpAddr>, allow_exempt: bool) -> bool {
        if self.parsed_allow_cidrs.len() != self.allow_cidrs.len()
            || self.parsed_deny_cidrs.len() != self.deny_cidrs.len()
        {
            return false;
        }
        let Some(address) = address else {
            return !self.enabled();
        };
        if self
            .parsed_deny_cidrs
            .iter()
            .any(|network| network.contains(&address))
        {
            return false;
        }
        allow_exempt
            || self.parsed_allow_cidrs.is_empty()
            || self
                .parsed_allow_cidrs
                .iter()
                .any(|network| network.contains(&address))
    }
}

fn parse_cidrs(field: &'static str, entries: &[String]) -> Result<Vec<IpNet>, ConfigError> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry.parse().map_err(|error: ipnet::AddrParseError| {
                ConfigError::InvalidAccessControlCidr {
                    field,
                    index,
                    value: entry.clone(),
                    message: error.to_string(),
                }
            })
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LimitsConfig {
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_header_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_url_length: Option<usize>,
}

pub const fn default_max_request_bytes() -> usize {
    32 * 1024 * 1024
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: default_max_request_bytes(),
            max_request_header_bytes: None,
            max_url_length: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TimeoutsConfig {
    #[serde(default = "default_upstream_ttfb_ms")]
    pub upstream_ttfb_ms: u64,
}

const fn default_upstream_ttfb_ms() -> u64 {
    120_000
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            upstream_ttfb_ms: default_upstream_ttfb_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub max: u32,
    pub window_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RateLimitsConfig {
    #[serde(default = "default_device_authorization_rate_limit")]
    pub device_authorization: RateLimitConfig,
    #[serde(default = "default_device_verify_rate_limit")]
    pub device_verify: RateLimitConfig,
}

fn default_device_authorization_rate_limit() -> RateLimitConfig {
    RateLimitConfig {
        max: 30,
        window_seconds: 600,
    }
}

fn default_device_verify_rate_limit() -> RateLimitConfig {
    RateLimitConfig {
        max: 10,
        window_seconds: 600,
    }
}

impl Default for RateLimitsConfig {
    fn default() -> Self {
        Self {
            device_authorization: default_device_authorization_rate_limit(),
            device_verify: default_device_verify_rate_limit(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct HttpTuning {
        #[serde(default)]
        access_control: AccessControlConfig,
        #[serde(default)]
        limits: LimitsConfig,
        #[serde(default)]
        timeouts: TimeoutsConfig,
        #[serde(default)]
        rate_limits: RateLimitsConfig,
    }

    #[test]
    fn defaults_match_the_documented_http_tuning_values() {
        let parsed: HttpTuning = toml::from_str("").unwrap();
        assert!(parsed.access_control.allow_cidrs.is_empty());
        assert!(parsed.access_control.deny_cidrs.is_empty());
        assert!(!parsed.access_control.trust_forwarded_for);
        assert_eq!(parsed.limits.max_request_bytes, 33_554_432);
        assert_eq!(parsed.limits.max_request_header_bytes, None);
        assert_eq!(parsed.limits.max_url_length, None);
        assert_eq!(parsed.timeouts.upstream_ttfb_ms, 120_000);
        assert_eq!(parsed.rate_limits.device_authorization.max, 30);
        assert_eq!(parsed.rate_limits.device_authorization.window_seconds, 600);
        assert_eq!(parsed.rate_limits.device_verify.max, 10);
        assert_eq!(parsed.rate_limits.device_verify.window_seconds, 600);
    }

    #[test]
    fn parses_all_http_tuning_tables_from_toml() {
        let parsed: HttpTuning = toml::from_str(
            r#"
                [access_control]
                allow_cidrs = ["10.0.0.0/8"]
                deny_cidrs = ["10.1.0.0/16"]
                trust_forwarded_for = true
                [limits]
                max_request_bytes = 1024
                max_request_header_bytes = 512
                max_url_length = 256
                [timeouts]
                upstream_ttfb_ms = 900
                [rate_limits.device_authorization]
                max = 3
                window_seconds = 20
                [rate_limits.device_verify]
                max = 2
                window_seconds = 30
            "#,
        )
        .unwrap();
        assert_eq!(parsed.access_control.allow_cidrs, ["10.0.0.0/8"]);
        assert_eq!(parsed.limits.max_request_bytes, 1024);
        assert_eq!(parsed.limits.max_request_header_bytes, Some(512));
        assert_eq!(parsed.limits.max_url_length, Some(256));
        assert_eq!(parsed.timeouts.upstream_ttfb_ms, 900);
        assert_eq!(parsed.rate_limits.device_authorization.max, 3);
        assert_eq!(parsed.rate_limits.device_verify.window_seconds, 30);
    }
}
