//! The dependency envelope: every route one requested id can reach before the
//! router has decided anything (ADR-0005 §3, issue #594).
//!
//! What this module guards is the **order** of admission and routing. Today
//! `proxy::failover` resolves a chain and then authenticates against it,
//! because the auth gate needs the chain to know whether a route injects a
//! credential. A driven router cannot keep that order: resolving its chain
//! means consulting a judge, which spends a gateway-held credential and the
//! judge target's pool quota — on behalf of a caller who may be about to be
//! rejected. So the gate is moved off the *decided* chain and onto this static
//! one: the requested id plus every id its router can name, answer targets and
//! judges alike, each expanded through the same ladder the request path uses.
//! The caller must authenticate if *any* member injects a credential, which is
//! why a passthrough answer tier paired with a credential-injecting judge is
//! not a passthrough entry.
//!
//! No deduplication and no ordering guarantee beyond "own chain, then targets,
//! then judges": the only question asked of this list is `any(!passthrough)`,
//! and a set operation to answer an `any` would be work the hot path does not
//! need. Nothing here makes a model call.

use crate::config::Config;
use crate::routing::{resolve_model_chain, strip_context_window_hint, Route};

/// Every route admission must consider for one requested id (ADR-0005 §3).
///
/// For an id that carries no router this is exactly
/// [`resolve_model_chain`] — the same chain `proxy::failover` gates against
/// today, so the unrouted path is unchanged and pays nothing.
pub(crate) fn dependency_envelope(config: &Config, model: &str) -> Vec<Route> {
    let model = strip_context_window_hint(model);
    let Some(router) = config
        .models
        .iter()
        .find(|configured| configured.id == model)
        .and_then(|configured| configured.router.as_ref())
    else {
        return resolve_model_chain(config, model);
    };
    // The entry's own body-less chain first: for a two-tier router that is the
    // picker default's chain, and for `type = "noop"` it is the single noop
    // route — which is never passthrough, so a noop entry's envelope demands
    // the credential exactly as the served request would.
    let mut envelope = resolve_model_chain(config, model);
    for id in router.targets().into_iter().chain(router.judges()) {
        envelope.extend(resolve_model_chain(config, id));
    }
    envelope
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{
        AuthMode, Config, ModelConfig, ProviderConfig, RouterConfig, StageClassifierConfig,
        StageRouterConfig, StageRouterPicker,
    };

    fn provider(auth: AuthMode) -> ProviderConfig {
        // Built from the shipped `anthropic` entry rather than field by field:
        // this test is about `auth`, and spelling out twenty unrelated provider
        // keys would make it read as if they mattered.
        let mut provider = Config::default()
            .providers
            .remove("anthropic")
            .expect("the default config ships an anthropic provider");
        provider.auth = auth;
        provider
    }

    fn mapped(id: &str, upstream: &str) -> ModelConfig {
        ModelConfig {
            subagents: None,
            id: id.to_string(),
            display_name: None,
            upstream_model: Some(BTreeMap::from([(
                upstream.to_string(),
                format!("{id}-upstream"),
            )])),
            router: None,
            stage_router: None,
        }
    }

    fn router(classifier: Option<&str>) -> StageRouterConfig {
        StageRouterConfig {
            classifier: classifier.map(|target| StageClassifierConfig {
                target: target.to_string(),
                base_threshold: 0.5,
            }),
            ..StageRouterConfig::preset(
                "capable-alias".to_string(),
                "efficient-alias".to_string(),
                StageRouterPicker::EfficientFirst,
                0.5,
            )
        }
    }

    /// `open` is passthrough, `keyed` injects, and `default` is the fall-through
    /// provider — the three shapes the assertions below need to tell apart.
    fn config(classifier: Option<&str>, default_provider: &str) -> Config {
        let mut config = Config {
            models: vec![
                ModelConfig {
                    subagents: None,
                    id: "claude-auto".to_string(),
                    display_name: None,
                    upstream_model: None,
                    router: Some(RouterConfig::StageRouter(router(classifier))),
                    stage_router: None,
                },
                mapped("capable-alias", "open"),
                mapped("efficient-alias", "open"),
                mapped("judge-alias", "keyed"),
            ],
            ..Config::default()
        };
        config.server.default_provider = default_provider.to_string();
        config.providers = BTreeMap::from([
            ("open".to_string(), provider(AuthMode::Passthrough)),
            ("keyed".to_string(), provider(AuthMode::ApiKey)),
        ]);
        config
    }

    /// The shape the whole ordering exists for: both answer tiers are
    /// passthrough, the judge is not, so the envelope carries a
    /// credential-injecting route and the auth gate demands a credential. Drop
    /// the `judges()` chain from `dependency_envelope` and this goes red.
    #[test]
    fn an_injecting_judge_joins_a_passthrough_answer_envelope() {
        let config = config(Some("judge-alias"), "open");
        let envelope = dependency_envelope(&config, "claude-auto");
        assert!(
            envelope.iter().any(|route| route.provider == "keyed"),
            "the judge's injecting route must be a member: {envelope:?}"
        );
        assert!(
            !envelope
                .iter()
                .all(|route| config.route_is_passthrough(route)),
            "an envelope that is passthrough throughout would skip [server.auth]"
        );
    }

    /// A judge that names no `[[models]]` entry is not absent from the
    /// envelope: it falls through the ladder to `server.default_provider`, and
    /// is a member with *that* provider's credential behaviour.
    #[test]
    fn a_judge_naming_no_entry_contributes_the_default_provider() {
        let config = config(Some("no-such-entry"), "keyed");
        let envelope = dependency_envelope(&config, "claude-auto");
        assert!(
            envelope
                .iter()
                .any(|route| route.provider == "keyed" && route.upstream_model == "no-such-entry"),
            "the judge must resolve through the default provider: {envelope:?}"
        );
    }

    /// The unrouted path is untouched: no router means the envelope *is* the
    /// chain, element for element.
    #[test]
    fn an_unrouted_id_yields_exactly_its_chain() {
        let config = config(None, "open");
        assert_eq!(
            dependency_envelope(&config, "capable-alias"),
            resolve_model_chain(&config, "capable-alias")
        );
    }
}
