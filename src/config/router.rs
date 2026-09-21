//! `[models.router]` — the routing-algorithm discriminator on a `[[models]]`
//! entry (ADR-0005 §2).
//!
//! One sub-table, internally tagged on `type`, rather than one sibling table
//! per algorithm: nine tables would need pairwise exclusion rules and every new
//! type would add eight of them. shunt's own config already discriminates this
//! way — `[[upstreams]] auth = { mode = … }` is an internally tagged enum with
//! `deny_unknown_fields` ([`crate::config::AuthMap`]) — so the shape is the
//! local idiom, not an import.
//!
//! `type` rather than shunt's usual `kind`/`mode` is a deliberate exception,
//! taken so the upstream Switchyard TOML schema's own values and documentation
//! quote unchanged.
//!
//! There is no `[models.stage_router]` alias. That table never appeared in a
//! release, so there is nothing deployed to migrate and no deprecation window
//! to keep open; a config still writing it is rejected at load by
//! [`crate::config::ConfigError::RemovedStageRouterTable`], which names the
//! replacement.

mod bounds;
mod prefill;
mod random;
mod stage;

pub use bounds::{
    CallBounds, DEFAULT_GATED_IDLE_MS, DEFAULT_GATED_MAX_BYTES, DEFAULT_GATED_MAX_DURATION_MS,
    DEFAULT_JUDGE_MAX_RESPONSE_BYTES, DEFAULT_JUDGE_TIMEOUT_MS, DEFAULT_MAX_JUDGE_CALLS,
};
pub use prefill::PrefillRouterConfig;
pub use random::{RandomAffinity, RandomRouterConfig};
pub use stage::{
    HandoffNotesConfig, StageClassifierConfig, StageRouterConfig, StageRouterPicker,
    ToolSemanticsConfig, DEFAULT_BASE_THRESHOLD, DEFAULT_CONFIDENCE_THRESHOLD,
    DEFAULT_DEESCALATE_THRESHOLD,
};

use serde::{Deserialize, Serialize};

/// The routing algorithm one `[[models]]` entry runs.
///
/// `deny_unknown_fields` is carried by each variant's own payload rather than
/// only by this container: serde strips the `type` key before handing the rest
/// of the table to a newtype variant, so a misspelled key inside
/// `[models.router]` is rejected by the payload struct. [`RouterConfig::Noop`]
/// is a struct variant with no fields, so the container's own
/// `deny_unknown_fields` is what rejects a stray key there — the same split
/// [`crate::config::AuthMap`] relies on.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterConfig {
    /// Signal-only tier selection (ADR-0004).
    StageRouter(StageRouterConfig),
    /// Upstream's stage-router preset: two targets, `efficient_first`, `0.5`.
    Auto(AutoRouterConfig),
    /// A weighted split across public model ids.
    Random(RandomRouterConfig),
    /// Upstream's learned prefill router: a checkpoint scores the latest text
    /// user turn. Compiled in only under `--features prefill-router`; the table
    /// parses either way (see [`PrefillRouterConfig`]).
    PrefillRouter(PrefillRouterConfig),
    /// Makes no upstream call and synthesizes an empty terminal assistant
    /// message in the caller's mode.
    Noop {},
}

impl RouterConfig {
    /// The stage table this router runs on, for the two types that have one.
    ///
    /// [`RouterConfig::Auto`] returns the pre-built preset it carries rather
    /// than constructing one, so the routed path pays no per-request
    /// construction for the preset form.
    pub fn stage(&self) -> Option<&StageRouterConfig> {
        match self {
            Self::StageRouter(stage) => Some(stage),
            Self::Auto(auto) => Some(auto.stage()),
            Self::Random(_) | Self::PrefillRouter(_) | Self::Noop {} => None,
        }
    }

    /// The `type` value, as a closed metric and `/routes` label.
    pub fn algorithm(&self) -> &'static str {
        match self {
            Self::StageRouter(_) => "stage_router",
            Self::Auto(_) => "auto",
            Self::Random(_) => "random",
            Self::PrefillRouter(_) => "prefill_router",
            Self::Noop {} => "noop",
        }
    }

    /// Every public model id this router can name, in declared order.
    ///
    /// **Answer targets only** — the ids a client request can actually be
    /// served by, which is what `/routes` reports as `targets` and what the
    /// unresolvable-target warning words itself around. A judge is enumerated
    /// by [`RouterConfig::named_judges`] instead, and validation ranges over
    /// both chained together so neither escapes the one-hop rule.
    ///
    /// The one-hop rule and the unresolvable-target warning both range over
    /// this, so a new algorithm that forgets to list a target here fails those
    /// checks silently — which is why every arm is spelled out rather than
    /// defaulting to empty.
    pub fn targets(&self) -> Vec<&str> {
        self.named_targets()
            .into_iter()
            .map(|(_, target)| target)
            .collect()
    }

    /// The same targets, each paired with the config key that named it, so a
    /// rejection can quote the key the operator wrote rather than a generic
    /// "target".
    pub fn named_targets(&self) -> Vec<(&'static str, &str)> {
        match self {
            Self::StageRouter(stage) => stage.named_targets(),
            Self::Auto(auto) => auto.stage().named_targets(),
            Self::Random(random) => random
                .targets
                .iter()
                .map(|target| ("targets", target.as_str()))
                .collect(),
            Self::PrefillRouter(prefill) => prefill
                .targets
                .iter()
                .map(|target| ("targets", target.as_str()))
                .collect(),
            // A noop entry answers as itself and names no destination.
            Self::Noop {} => Vec::new(),
        }
    }

    /// Every id this router *consults* but never serves, paired with the key
    /// that named it (ADR-0005 §7).
    ///
    /// Kept apart from [`RouterConfig::named_targets`] rather than folded into
    /// it because the two are read for different reasons: a target is a place a
    /// client turn can land, a judge is not. Validation chains them, `/routes`
    /// reports them as separate lists, and the dependency envelope appends the
    /// judges after the targets.
    pub fn named_judges(&self) -> Vec<(&'static str, &str)> {
        match self.stage_classifier() {
            Some((_, classifier)) => vec![("classifier.target", classifier.target.as_str())],
            None => Vec::new(),
        }
    }

    /// The judge ids alone, in declared order.
    pub fn judges(&self) -> Vec<&str> {
        self.named_judges()
            .into_iter()
            .map(|(_, judge)| judge)
            .collect()
    }

    /// The stage table and its classifier, for the two types that can carry
    /// one. `None` for every entry that runs on the pure lane.
    pub fn stage_classifier(&self) -> Option<(&StageRouterConfig, &StageClassifierConfig)> {
        let stage = self.stage()?;
        stage.classifier.as_ref().map(|c| (stage, c))
    }

    /// Whether this router makes model calls while routing — today, exactly
    /// "it carries a classifier" (ADR-0005 §1). The gate the request path reads
    /// before it computes a dependency envelope or constructs a driver at all,
    /// so a pure-lane entry pays nothing for the driven lane's existence.
    pub fn is_driven(&self) -> bool {
        self.stage_classifier().is_some()
    }
}

/// `[models.router]` with `type = "auto"` — upstream's stage-router preset.
///
/// Exactly two keys on the wire, and a whole [`StageRouterConfig`] in memory:
/// the preset is built once at load, so a routed request reads a ready table
/// instead of assembling one per turn. `Serialize` goes back through
/// [`AutoRouterTable`], so a round-tripped config emits the two keys the
/// operator wrote rather than the expanded stage table.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(from = "AutoRouterTable", into = "AutoRouterTable")]
pub struct AutoRouterConfig {
    stage: StageRouterConfig,
}

impl AutoRouterConfig {
    /// The preset stage table, built at load.
    pub fn stage(&self) -> &StageRouterConfig {
        &self.stage
    }
}

/// The two keys `type = "auto"` accepts on the wire.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AutoRouterTable {
    capable_target: String,
    efficient_target: String,
}

impl From<AutoRouterTable> for AutoRouterConfig {
    fn from(table: AutoRouterTable) -> Self {
        Self {
            stage: StageRouterConfig::preset(
                table.capable_target,
                table.efficient_target,
                StageRouterPicker::EfficientFirst,
                DEFAULT_CONFIDENCE_THRESHOLD,
            ),
        }
    }
}

impl From<AutoRouterConfig> for AutoRouterTable {
    fn from(auto: AutoRouterConfig) -> Self {
        Self {
            capable_target: auto.stage.capable_target,
            efficient_target: auto.stage.efficient_target,
        }
    }
}
