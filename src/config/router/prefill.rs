//! `[models.router] type = "prefill_router"` — upstream's learned prefill
//! router (ADR-0005 §6).
//!
//! The keys are upstream's verbatim (`docs/reference/toml_schema.md`
//! §`prefill_router`), including the two whose defaults live in the upstream
//! crate rather than here: `max_length` (2048) and `batch_size` (32) stay
//! `Option` so an unset key keeps whatever upstream's
//! `TransformersForwardConfig` chooses, instead of freezing a copy of it into
//! shunt's config that would drift on the next rev bump.
//!
//! This table parses in **both** builds. The algorithm behind it only exists
//! under `--features prefill-router`, but the schema must not change with the
//! feature: a config written against a from-source build has to be readable by
//! a release binary, which rejects it by name
//! ([`crate::config::ConfigError::PrefillRouterFeatureDisabled`]) rather than
//! with an "unknown variant" parse error that names no remedy.
//!
//! Policy and a filesystem path only: no credentials live here, so a derived
//! `Debug` cannot leak one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `[models.router]` with `type = "prefill_router"`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrefillRouterConfig {
    /// Public model ids in the exact order of the checkpoint's output heads.
    ///
    /// Positional, not named: upstream binds head *i* to target *i*, so
    /// reordering this list silently re-labels every prediction.
    pub targets: Vec<String>,
    /// Tensor-only router checkpoint path; relative paths resolve against the
    /// process cwd, the same rule upstream documents.
    pub checkpoint: PathBuf,
    /// Torch device for encoder inference (`cpu`, `cuda`, `cuda:0`, …).
    /// Auto-detected when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Hugging Face cache directory for the downloaded encoder and tokenizer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Maximum tokenized encoder input length; longer prompts are truncated.
    /// Upstream's default is 2048.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    /// Maximum prompts per encoder forward pass. Upstream's default is 32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<usize>,
}

impl PrefillRouterConfig {
    /// Upstream's default target: the first head.
    ///
    /// That is not a shunt convention — `PrefillRouterAlgo` keeps
    /// `targets.first()` as its own `default_target` and routes a turn with no
    /// text user message there — so the fail-open and body-less answers have to
    /// name the same id or the two lanes would disagree about the same config.
    /// `None` only for an unvalidated empty list, which `validate` refuses.
    pub fn default_target(&self) -> Option<&str> {
        self.targets.first().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The upstream schema's own example, minus the keys that are upstream's
    /// route-table wrapper rather than the algorithm's.
    #[test]
    fn the_upstream_example_parses_with_the_optional_keys_unset() {
        let router: PrefillRouterConfig = toml::from_str(
            r#"
targets = ["fast", "strong"]
checkpoint = "/models/router.pt"
"#,
        )
        .expect("the upstream example parses");

        assert_eq!(router.targets, vec!["fast", "strong"]);
        assert_eq!(router.checkpoint, PathBuf::from("/models/router.pt"));
        assert_eq!(router.device, None);
        assert_eq!(router.cache_dir, None);
        assert_eq!(router.max_length, None);
        assert_eq!(router.batch_size, None);
        assert_eq!(router.default_target(), Some("fast"));
    }

    #[test]
    fn every_optional_key_parses_when_written() {
        let router: PrefillRouterConfig = toml::from_str(
            r#"
targets = ["fast", "strong"]
checkpoint = "router.pt"
device = "cuda:0"
cache_dir = "/var/cache/hf"
max_length = 512
batch_size = 8
"#,
        )
        .expect("every documented key is accepted");

        assert_eq!(router.device.as_deref(), Some("cuda:0"));
        assert_eq!(router.cache_dir, Some(PathBuf::from("/var/cache/hf")));
        assert_eq!(router.max_length, Some(512));
        assert_eq!(router.batch_size, Some(8));
    }

    /// `deny_unknown_fields` is what turns a typo into a load failure rather
    /// than a key that silently does nothing.
    #[test]
    fn an_unknown_key_is_rejected() {
        let error = toml::from_str::<PrefillRouterConfig>(
            r#"
targets = ["fast"]
checkpoint = "router.pt"
maxlength = 512
"#,
        )
        .expect_err("an unknown key is refused");

        assert!(
            error.to_string().contains("maxlength"),
            "the rejection must name the key the operator wrote: {error}"
        );
    }

    /// A round trip emits the keys the operator wrote and nothing else, so a
    /// serialized config does not grow defaults shunt does not own.
    #[test]
    fn a_round_trip_omits_the_absent_optional_keys() {
        let router = PrefillRouterConfig {
            targets: vec!["fast".to_string()],
            checkpoint: PathBuf::from("router.pt"),
            device: None,
            cache_dir: None,
            max_length: None,
            batch_size: None,
        };

        let value = serde_json::to_value(&router).expect("the table serializes");

        assert_eq!(
            value,
            serde_json::json!({"targets": ["fast"], "checkpoint": "router.pt"})
        );
        assert_eq!(
            serde_json::from_value::<PrefillRouterConfig>(value).expect("the table round-trips"),
            router
        );
    }
}
