//! Load-time rules for the driven `[models.router]` and `[models.subagents]`
//! types (ADR-0005 §8 PR 5, PR 6).
//!
//! These live beside the tables they check rather than in `config.rs` for one
//! reason: every rule here exists because *libsy* would otherwise raise it on
//! the first judged turn, and the point of raising it at load is that the
//! operator sees shunt's key names rather than upstream's field names. Keeping
//! the two next to each other is what makes a key rename a compile-time
//! coupling instead of a documentation exercise.
//!
//! The last check is deliberately not a re-implementation: after the key-level
//! verdicts, [`crate::routing::driven::check_buildable`] constructs the real
//! `LlmTaskClassifier`/`CompositeRouter` and drops it, so any rule upstream
//! enforces and shunt does not still fails `shunt check` — as
//! [`ConfigError::DrivenRouterBuild`] carrying upstream's own message — rather
//! than the first request.

use std::collections::BTreeMap;

use crate::config::{Config, ConfigError};

use super::{
    AdvisorRouterConfig, CallBounds, ClassifierPolicy, ClassifyTrigger, CompositeRouterConfig,
    CustomClassifierConfig, LlmClassifierConfig, ANY_GROUP, JUDGE_GROUP,
};
use crate::config::SubagentsClassifierConfig;

/// The keys a custom classifier shares between its router and overlay forms.
struct CustomGroups<'a> {
    models: &'a BTreeMap<String, Vec<String>>,
    default_target: &'a str,
    prompt: &'a str,
    response_schema: &'a str,
    policy: &'a ClassifierPolicy,
}

impl Config {
    /// `type = "llm_classifier"`, both modes.
    pub(crate) fn validate_llm_classifier(
        &self,
        model_id: &str,
        classifier: &LlmClassifierConfig,
    ) -> Result<(), ConfigError> {
        CallBounds::validate(model_id, classifier.bound_keys())?;
        match classifier {
            LlmClassifierConfig::Capability(capability) => {
                validate_threshold(model_id, "base_threshold", capability.base_threshold)?;
                validate_threshold_step(
                    model_id,
                    capability.base_threshold,
                    capability.threshold_step,
                )?;
                validate_judge_runtime(
                    model_id,
                    capability.max_output_tokens,
                    capability.recent_turn_window,
                    capability.message_hash_fallback,
                    capability.classify_trigger,
                )?;
            }
            LlmClassifierConfig::Custom(custom) => {
                validate_judge_runtime(
                    model_id,
                    custom.max_output_tokens,
                    custom.recent_turn_window,
                    custom.message_hash_fallback,
                    custom.classify_trigger,
                )?;
                validate_custom_groups(model_id, &groups_of(custom))?;
            }
            // The escalation table's own rules (`confirmations`, the window,
            // the character cap) are upstream's constructor's, reached through
            // the build check `validate_router` runs last. Only the key every
            // judge-backed mode shares is spelled here.
            LlmClassifierConfig::Escalation(escalation) => {
                if escalation.max_output_tokens == 0 {
                    return Err(ConfigError::ZeroMaxOutputTokens {
                        model: model_id.to_string(),
                    });
                }
            }
        }
        for (key, judge) in classifier.named_judges() {
            self.validate_judge_is_injecting(model_id, &key, judge)?;
        }
        Ok(())
    }

    /// `type = "composite"`.
    pub(crate) fn validate_composite(
        &self,
        model_id: &str,
        composite: &CompositeRouterConfig,
    ) -> Result<(), ConfigError> {
        CallBounds::validate(model_id, composite.bound_keys())?;
        validate_threshold(
            model_id,
            "classifier.base_threshold",
            composite.classifier.base_threshold,
        )?;
        validate_threshold(
            model_id,
            "stage.confidence_threshold",
            composite.stage.confidence_threshold,
        )?;
        if composite.stage.recent_turn_window == 0 {
            return Err(ConfigError::InvalidStageRouterWindow {
                model: model_id.to_string(),
            });
        }
        // No `message_hash_fallback` rule here, deliberately: the standalone
        // classifier pairs the fallback with `new_session` because its own
        // affinity map is what keys on the hash, but a composite implements the
        // retention itself and accepts the hash key under either trigger — the
        // only trigger it refuses, `every_request`, the `CompositeTrigger` type
        // cannot even spell. Raising the standalone rule here would refuse the
        // one way a client that sends no session id keeps its tier across tool
        // continuations.
        crate::config::validate_tool_semantics(model_id, &composite.stage.tool_semantics)?;
        for (key, judge) in composite.named_judges() {
            self.validate_judge_is_injecting(model_id, &key, judge)?;
        }
        Ok(())
    }

    /// `type = "advisor"`.
    ///
    /// The advisor is a judge — it reads the caller's transcript on the
    /// gateway's credential and serves nothing — so it is held to the judge
    /// rule. The executor is not: its gated turn is the answer dispatch and
    /// carries the caller's credential exactly as a live turn does, so a
    /// passthrough executor is a legitimate shape.
    pub(crate) fn validate_advisor(
        &self,
        model_id: &str,
        advisor: &AdvisorRouterConfig,
    ) -> Result<(), ConfigError> {
        CallBounds::validate(model_id, advisor.bound_keys())?;
        if let Some(reason) = advisor.trigger_pattern_problem() {
            return Err(ConfigError::InvalidAdvisorGatePattern {
                model: model_id.to_string(),
                reason,
            });
        }
        self.validate_judge_is_injecting(model_id, "advisor_target", &advisor.advisor_target)
    }

    /// `[models.subagents] type = "llm_classifier"`.
    ///
    /// The two extra rules over the router form are the ones the
    /// `(session, agent)` identity forces — see the module docs on
    /// [`crate::config::SubagentsClassifierConfig`].
    pub(crate) fn validate_subagents_classifier(
        &self,
        model_id: &str,
        classifier: &SubagentsClassifierConfig,
    ) -> Result<(), ConfigError> {
        let custom = classifier.custom();
        CallBounds::validate(model_id, classifier.bound_keys())?;
        if custom.classify_trigger == ClassifyTrigger::UserTurn {
            return Err(ConfigError::SubagentsClassifierTrigger {
                model: model_id.to_string(),
            });
        }
        if custom.message_hash_fallback {
            return Err(ConfigError::SubagentsMessageHashFallback {
                model: model_id.to_string(),
            });
        }
        validate_judge_runtime(
            model_id,
            custom.max_output_tokens,
            custom.recent_turn_window,
            // Refused outright above, so the shared rule can never fire here.
            false,
            custom.classify_trigger,
        )?;
        validate_custom_groups(
            model_id,
            &CustomGroups {
                models: &custom.models,
                default_target: &custom.default_target,
                prompt: &custom.prompt,
                response_schema: &custom.response_schema,
                policy: &custom.policy,
            },
        )?;
        for (key, judge) in classifier.named_judges() {
            self.validate_judge_is_injecting(model_id, &key, judge)?;
        }
        crate::routing::driven::check_subagents_buildable(classifier).map_err(|message| {
            ConfigError::DrivenRouterBuild {
                model: model_id.to_string(),
                message,
            }
        })
    }
}

fn groups_of<'a>(custom: &'a CustomClassifierConfig) -> CustomGroups<'a> {
    CustomGroups {
        models: &custom.models,
        default_target: &custom.default_target,
        prompt: &custom.prompt,
        response_schema: &custom.response_schema,
        policy: &custom.policy,
    }
}

/// The `(0, 1]` range every judge and scorer gate is held to.
///
/// Written as `!(v > 0.0 && v <= 1.0)` rather than a negated range so NaN,
/// which compares false against every bound, is rejected too.
fn validate_threshold(model_id: &str, key: &'static str, value: f64) -> Result<(), ConfigError> {
    if !(value > 0.0 && value <= 1.0) {
        return Err(ConfigError::InvalidStageRouterThreshold {
            model: model_id.to_string(),
            key,
            value,
        });
    }
    Ok(())
}

/// libsy's own rule, raised at load: an unsupported verdict costs two steps, so
/// `base + 2 * step` must still be a probability.
fn validate_threshold_step(model_id: &str, base: f64, step: f64) -> Result<(), ConfigError> {
    if !step.is_finite() || step < 0.0 || base + 2.0 * step > 1.0 {
        return Err(ConfigError::InvalidThresholdStep {
            model: model_id.to_string(),
            value: step,
        });
    }
    Ok(())
}

/// The three keys every judge-backed mode carries, whatever its shape.
fn validate_judge_runtime(
    model_id: &str,
    max_output_tokens: u64,
    recent_turn_window: Option<usize>,
    message_hash_fallback: bool,
    classify_trigger: ClassifyTrigger,
) -> Result<(), ConfigError> {
    if max_output_tokens == 0 {
        return Err(ConfigError::ZeroMaxOutputTokens {
            model: model_id.to_string(),
        });
    }
    // `Some(0)` is not "the default": it asks libsy to window the transcript
    // down to the opening task alone while still paying for the windowed
    // prompt shape. An operator who wants the narrow judge omits the key.
    if recent_turn_window == Some(0) {
        return Err(ConfigError::InvalidClassifierWindow {
            model: model_id.to_string(),
        });
    }
    if message_hash_fallback && classify_trigger != ClassifyTrigger::NewSession {
        return Err(ConfigError::MessageHashFallbackTrigger {
            model: model_id.to_string(),
        });
    }
    Ok(())
}

/// The group, prompt, schema, and selector rules a custom classifier shares
/// between its router and overlay forms.
fn validate_custom_groups(model_id: &str, groups: &CustomGroups<'_>) -> Result<(), ConfigError> {
    // `any` is upstream's runtime model list and `judge` is what the classifier
    // itself runs on: a config missing either declares an algorithm that cannot
    // be driven at all, so both are named rather than defaulted.
    for group in [ANY_GROUP, JUDGE_GROUP] {
        if groups.models.get(group).is_none_or(Vec::is_empty) {
            return Err(ConfigError::MissingClassifierGroup {
                model: model_id.to_string(),
                group,
            });
        }
    }
    let any = &groups.models[ANY_GROUP];
    // `judge` is exempt, and upstream's own example is why: its judge is a
    // small model that serves no turn, so it is deliberately absent from the
    // runtime list. Every *answer* group must be a subset of `any`, because
    // `any` is the list the driver resolves a selected category against.
    for (group, ids) in groups.models {
        if group == ANY_GROUP || group == JUDGE_GROUP {
            continue;
        }
        for id in ids {
            if !any.contains(id) {
                return Err(ConfigError::ClassifierTargetNotInAny {
                    model: model_id.to_string(),
                    group: group.clone(),
                    target: id.clone(),
                });
            }
        }
    }
    if groups.default_target == JUDGE_GROUP
        || groups
            .models
            .get(groups.default_target)
            .is_none_or(Vec::is_empty)
    {
        return Err(ConfigError::InvalidClassifierDefaultTarget {
            model: model_id.to_string(),
            group: groups.default_target.to_string(),
        });
    }
    if groups.prompt.trim().is_empty() {
        return Err(ConfigError::InvalidClassifierPrompt {
            model: model_id.to_string(),
            reason: "must not be blank",
        });
    }
    if groups.prompt.contains("{{RESPONSE_SCHEMA}}") {
        return Err(ConfigError::InvalidClassifierPrompt {
            model: model_id.to_string(),
            reason: "must not contain {{RESPONSE_SCHEMA}}; the schema is supplied automatically",
        });
    }
    match serde_json::from_str::<serde_json::Value>(groups.response_schema) {
        Ok(serde_json::Value::Object(_)) => {}
        Ok(_) => {
            return Err(ConfigError::InvalidResponseSchema {
                model: model_id.to_string(),
                message: "the schema must be a JSON object".to_string(),
            })
        }
        Err(error) => {
            return Err(ConfigError::InvalidResponseSchema {
                model: model_id.to_string(),
                message: error.to_string(),
            })
        }
    }
    let ClassifierPolicy::TargetSelector { selector } = groups.policy;
    if selector.trim().is_empty() {
        return Err(ConfigError::InvalidClassifierPrompt {
            model: model_id.to_string(),
            reason: "policy selector must not be blank",
        });
    }
    Ok(())
}
