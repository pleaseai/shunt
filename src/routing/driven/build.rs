//! Building the driven lane's algorithms (ADR-0005 §8 PR 5).
//!
//! Two callers, one construction. `Config::validate` asks *would this table
//! build an algorithm at all?* and answers it the only way that cannot drift —
//! by constructing the real `LlmTaskClassifier`/`CompositeRouter` and dropping
//! it — so every rule upstream enforces in its own constructor (a JSON Pointer
//! that is not one, a prompt carrying `{{RESPONSE_SCHEMA}}`, a threshold pair
//! that cannot span a probability, `every_request` on a composite entry) fails
//! `shunt check` rather than the first judged turn, and does so quoting
//! upstream's own message. [`RuntimeState`](crate::reload::RuntimeState) then
//! asks for the same algorithms to *keep*.
//!
//! The validation-time instance is thrown away on purpose. Affinity state
//! lives *inside* the instance, so the one a request is routed by has to be
//! long-lived and owned by `RuntimeState`; a validation-time instance that
//! escaped here would be a second, short-lived session map.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use switchyard_libsy::{
    Algorithm, ClassifierContractConfig, CompositeRouter,
    CompositeRouterConfig as LibsyCompositeConfig, CustomClassifierConfig as LibsyCustomConfig,
    CustomClassifierPolicy, LlmClassifierConfig, LlmTaskClassifier, PickerMode, RuntimeModels,
    StageRouterConfig as LibsyStageConfig, TaskClassifierConfig, ToolSemantics,
};
use switchyard_protocol::{Category, ModelId};

use crate::config::{
    CapabilityClassifierConfig, ClassifierPolicy, CompositeRouterConfig, CustomClassifierConfig,
    LlmClassifierConfig as ShuntClassifierConfig, RouterConfig, SubagentsClassifierConfig,
    ToolSemanticsConfig, DEFAULT_MAX_OUTPUT_TOKENS,
};
use crate::routing::driven::{budget::JudgeBudget, DrivenEntry};

/// Build the algorithm a driven `[models.router]` names and drop it, reporting
/// upstream's message on failure.
///
/// Takes the whole [`RouterConfig`] rather than a payload so a driven type
/// added later cannot reach the request path without an arm here: the `_` arm
/// below is the pure lane, which builds nothing.
pub(crate) fn check_buildable(router: &RouterConfig) -> Result<(), String> {
    match router {
        RouterConfig::LlmClassifier(classifier) => build_classifier(classifier).map(drop),
        RouterConfig::Composite(composite) => build_composite(composite).map(drop),
        _ => Ok(()),
    }
}

/// The same check for a classifier-form `[models.subagents]` overlay.
pub(crate) fn check_subagents_buildable(
    classifier: &SubagentsClassifierConfig,
) -> Result<(), String> {
    let custom = classifier.custom();
    let config = custom_config(
        &custom.prompt,
        &custom.response_schema,
        &custom.policy,
        custom.classify_trigger.to_libsy(),
        custom.message_hash_fallback,
        custom.recent_turn_window,
        custom.max_output_tokens,
    )?;
    LlmTaskClassifier::new(LlmClassifierConfig::Custom {
        default_target: category(&custom.default_target)?,
        config,
    })
    .map(drop)
    .map_err(|error| error.to_string())
}

fn build_classifier(classifier: &ShuntClassifierConfig) -> Result<LlmTaskClassifier, String> {
    let config = match classifier {
        ShuntClassifierConfig::Capability(capability) => LlmClassifierConfig::Capability {
            config: capability_config(capability),
        },
        ShuntClassifierConfig::Custom(custom) => LlmClassifierConfig::Custom {
            default_target: category(&custom.default_target)?,
            config: custom_config_of(custom)?,
        },
    };
    LlmTaskClassifier::new(config).map_err(|error| error.to_string())
}

fn build_composite(composite: &CompositeRouterConfig) -> Result<CompositeRouter, String> {
    let mut stage = LibsyStageConfig::new(
        // No `picker` key: the judge supplies the fall-open tier, so the
        // efficient-first base is what the verdict then raises.
        PickerMode::EfficientFirst,
        composite.stage.confidence_threshold,
    );
    stage.recent_window = Some(composite.stage.recent_turn_window);
    stage.tool_semantics = tool_semantics(&composite.stage.tool_semantics);
    stage.capable_hold_turns = composite.stage.capable_hold_turns;
    CompositeRouter::new(LibsyCompositeConfig {
        judge: TaskClassifierConfig {
            base_threshold: composite.classifier.base_threshold,
            threshold_step: 0.0,
            classify_trigger: composite
                .classifier
                .classify_trigger
                .as_classify_trigger()
                .to_libsy(),
            message_hash_fallback: composite.classifier.message_hash_fallback,
            recent_turn_window: None,
            contract: ClassifierContractConfig::default(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        },
        stage,
    })
    .map_err(|error| error.to_string())
}

fn capability_config(capability: &CapabilityClassifierConfig) -> TaskClassifierConfig {
    TaskClassifierConfig {
        base_threshold: capability.base_threshold,
        threshold_step: capability.threshold_step,
        classify_trigger: capability.classify_trigger.to_libsy(),
        message_hash_fallback: capability.message_hash_fallback,
        recent_turn_window: capability.recent_turn_window,
        // `response_format_type` is private upstream and only reachable through
        // `Default`, which is `JsonSchema` — the mode the live capture was
        // taken against.
        contract: capability
            .prompt
            .as_ref()
            .map_or_else(ClassifierContractConfig::default, |prompt| {
                ClassifierContractConfig::default().with_prompt(prompt.clone())
            }),
        max_output_tokens: capability.max_output_tokens,
    }
}

fn custom_config_of(custom: &CustomClassifierConfig) -> Result<LibsyCustomConfig, String> {
    custom_config(
        &custom.prompt,
        &custom.response_schema,
        &custom.policy,
        custom.classify_trigger.to_libsy(),
        custom.message_hash_fallback,
        custom.recent_turn_window,
        custom.max_output_tokens,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the router and overlay forms are separate config types with the same seven custom-classifier keys; one parameter list is what keeps their construction identical"
)]
fn custom_config(
    prompt: &str,
    response_schema: &str,
    policy: &ClassifierPolicy,
    classify_trigger: switchyard_libsy::ClassifyTrigger,
    message_hash_fallback: bool,
    recent_turn_window: Option<usize>,
    max_output_tokens: u64,
) -> Result<LibsyCustomConfig, String> {
    let schema: Value = serde_json::from_str(response_schema)
        .map_err(|error| format!("response_schema is not valid JSON: {error}"))?;
    let ClassifierPolicy::TargetSelector { selector } = policy;
    let mut config = LibsyCustomConfig::new(
        prompt,
        schema,
        CustomClassifierPolicy::target_selector(selector.clone()),
    );
    config.classify_trigger = classify_trigger;
    config.message_hash_fallback = message_hash_fallback;
    config.recent_turn_window = recent_turn_window;
    config.max_output_tokens = max_output_tokens;
    Ok(config)
}

/// A group name as libsy's category. The four reserved names (`any`,
/// `capable`, `efficient`, `judge`) map to their own variants and everything
/// else becomes `Named`.
fn category(group: &str) -> Result<Category, String> {
    group.parse::<Category>()
}

/// shunt's declared tool semantics as libsy's.
///
/// Cloned rather than borrowed because libsy's type owns its lists; the call
/// happens once per load and once per `RuntimeState`, never per request.
fn tool_semantics(semantics: &ToolSemanticsConfig) -> ToolSemantics {
    ToolSemantics {
        observe: semantics.observe.clone(),
        mutate: semantics.mutate.clone(),
        plan: semantics.plan.clone(),
        new: semantics.new.clone(),
    }
}

/// The long-lived entry for a driven `[models.router]`, or `None` for a
/// pure-lane router.
///
/// Built once per [`RuntimeState`](crate::reload::RuntimeState), never per
/// request: libsy's affinity map and a composite's retained tiers live inside
/// the algorithm instance, so a per-request build would forget the session
/// every turn — which is exactly the state `classify_trigger` exists to keep.
pub(super) fn router_entry(router: &RouterConfig) -> Result<Option<DrivenEntry>, String> {
    let entry = match router {
        RouterConfig::LlmClassifier(classifier) => {
            let algorithm = Arc::new(build_classifier(classifier)?) as Arc<dyn Algorithm>;
            DrivenEntry {
                algorithm,
                models: Arc::new(RuntimeModels::new(classifier_models(classifier)?)),
                targets: target_ids(classifier.named_targets()),
                fail_open: classifier.fail_open_target().to_string(),
                bounds: classifier.bounds(),
                algorithm_label: router.algorithm(),
                budget: JudgeBudget::new(),
                tiers: None,
            }
        }
        RouterConfig::Composite(composite) => {
            let algorithm = Arc::new(build_composite(composite)?) as Arc<dyn Algorithm>;
            let capable = composite.stage.capable_target.clone();
            let efficient = composite.stage.efficient_target.clone();
            DrivenEntry {
                algorithm,
                models: Arc::new(RuntimeModels::new(HashMap::from([
                    (
                        Category::Judge,
                        vec![ModelId::from(composite.classifier.target.as_str())],
                    ),
                    (Category::Capable, vec![ModelId::from(capable.as_str())]),
                    (Category::Efficient, vec![ModelId::from(efficient.as_str())]),
                    // `Any` is the list upstream's `FallThrough` checks a
                    // selected target against before it publishes an outcome
                    // (`ensure_model_is_target`), so a route whose tiers are
                    // absent from it errors instead of answering. The judge is
                    // deliberately not a member: it is consulted, never served.
                    (
                        Category::Any,
                        vec![
                            ModelId::from(capable.as_str()),
                            ModelId::from(efficient.as_str()),
                        ],
                    ),
                ]))),
                targets: vec![capable.clone(), efficient.clone()],
                fail_open: composite.fail_open_target().to_string(),
                bounds: composite.bounds(),
                algorithm_label: router.algorithm(),
                budget: JudgeBudget::new(),
                tiers: Some((capable, efficient)),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(entry))
}

/// The same for a classifier-form `[models.subagents]` overlay.
///
/// One mode (`custom`), so this is the custom construction with the overlay's
/// own keys — and the `"subagents"` label, which is what the overlay reports
/// whichever form it is in.
pub(super) fn overlay_entry(classifier: &SubagentsClassifierConfig) -> Result<DrivenEntry, String> {
    let custom = classifier.custom();
    let config = custom_config(
        &custom.prompt,
        &custom.response_schema,
        &custom.policy,
        custom.classify_trigger.to_libsy(),
        custom.message_hash_fallback,
        custom.recent_turn_window,
        custom.max_output_tokens,
    )?;
    let algorithm = LlmTaskClassifier::new(LlmClassifierConfig::Custom {
        default_target: category(&custom.default_target)?,
        config,
    })
    .map_err(|error| error.to_string())?;
    Ok(DrivenEntry {
        algorithm: Arc::new(algorithm) as Arc<dyn Algorithm>,
        models: Arc::new(RuntimeModels::new(group_models(&custom.models)?)),
        targets: target_ids(classifier.named_targets()),
        fail_open: classifier.fail_open_target().to_string(),
        bounds: classifier.bounds(),
        algorithm_label: "subagents",
        budget: JudgeBudget::new(),
        tiers: None,
    })
}

/// The ids a verdict may name, with the config keys that named them dropped:
/// the request path asks only "is the selected id one of these?".
fn target_ids(named: Vec<(Cow<'static, str>, &str)>) -> Vec<String> {
    named
        .into_iter()
        .map(|(_, target)| target.to_string())
        .collect()
}

/// The `RuntimeModels` groups for a `[models.router] type = "llm_classifier"`.
///
/// Capability mode publishes the categories libsy's packaged route reads —
/// `Judge` for the consulted id, the two tiers it decides between, and `Any`,
/// which is the list `FallThrough` validates the selected target against — and
/// custom mode publishes the operator's own groups verbatim, `any` included,
/// because `any` *is* that list.
fn classifier_models(
    classifier: &ShuntClassifierConfig,
) -> Result<HashMap<Category, Vec<ModelId>>, String> {
    match classifier {
        ShuntClassifierConfig::Capability(capability) => Ok(HashMap::from([
            (
                Category::Judge,
                vec![ModelId::from(capability.classifier_target.as_str())],
            ),
            (
                Category::Capable,
                vec![ModelId::from(capability.strong_target.as_str())],
            ),
            (
                Category::Efficient,
                vec![ModelId::from(capability.weak_target.as_str())],
            ),
            // See the composite arm: `Any` is what upstream's `FallThrough`
            // validates a selected target against, and a custom entry supplies
            // it by hand (validation requires the group). The two tiers only —
            // the judge is consulted, never served.
            (
                Category::Any,
                vec![
                    ModelId::from(capability.strong_target.as_str()),
                    ModelId::from(capability.weak_target.as_str()),
                ],
            ),
        ])),
        ShuntClassifierConfig::Custom(custom) => group_models(&custom.models),
    }
}

/// A `models` table as libsy's categories. The four reserved names map to
/// their own variants and every other group becomes `Named`, which is what the
/// selector policy resolves a verdict's string against.
fn group_models(
    models: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<HashMap<Category, Vec<ModelId>>, String> {
    models
        .iter()
        .map(|(group, ids)| {
            Ok((
                category(group)?,
                ids.iter().map(|id| ModelId::from(id.as_str())).collect(),
            ))
        })
        .collect()
}
