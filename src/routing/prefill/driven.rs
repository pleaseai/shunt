//! The feature-on half of [`super`]: building upstream's `PrefillRouterAlgo`
//! and driving one request through `switchyard_libsy::drive`.
//!
//! Every upstream type this PR touches is named here and nowhere else, so a
//! build without the `prefill-router` feature never mentions them.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::Value;
use switchyard_libsy::{drive, Algorithm, LibsyError, RuntimeModels};
use switchyard_protocol::{
    Category, ContentBlock, LlmRequest, Message, Metadata, ModelId, Request, Role, ToolResult,
};

use crate::config::{Config, ConfigError, RouterConfig};
use crate::routing::context::RouterContext;
use crate::routing::outcome::{PrefillDecision, RouteSource};
use crate::routing::strip_context_window_hint;

/// One built entry: the algorithm plus the two things a drive needs that the
/// algorithm does not hand back — the target list to publish as its
/// [`RuntimeModels`], and the id to fall open to.
pub(super) struct Entry {
    algorithm: Arc<dyn Algorithm>,
    /// The configured targets as libsy ids, kept in checkpoint-head order.
    /// Cloned per request into `RuntimeModels`, which takes ownership.
    targets: Vec<ModelId>,
    /// Upstream's own default: the first target. Both the fail-open answer and
    /// upstream's "no text user message" answer name it, so they agree.
    default_target: String,
}

impl Entry {
    /// The test seam behind [`super::PrefillRouters::from_algorithm`].
    #[cfg(test)]
    pub(super) fn new(algorithm: Arc<dyn Algorithm>, targets: &[&str]) -> Self {
        Self {
            algorithm,
            targets: targets.iter().copied().map(ModelId::from).collect(),
            default_target: targets.first().copied().unwrap_or_default().to_string(),
        }
    }
}

/// Build one algorithm per `prefill_router` entry.
///
/// `cfg.build()` runs `Python::attach`: it loads the embedded module and the
/// checkpoint's metadata. The encoder itself is loaded lazily by upstream on
/// the first inference, so a gateway that boots successfully has proved the
/// checkpoint is readable but not yet that torch can run it.
pub(super) fn build(config: &Config) -> Result<HashMap<String, Entry>, ConfigError> {
    let mut by_model = HashMap::new();
    for model in &config.models {
        let Some(RouterConfig::PrefillRouter(prefill)) = model.router.as_ref() else {
            continue;
        };
        let targets: Vec<ModelId> = prefill
            .targets
            .iter()
            .map(|t| ModelId::from(t.as_str()))
            .collect();
        // Validation guarantees a non-empty list, so the default exists.
        let default_target = prefill
            .default_target()
            .unwrap_or(model.id.as_str())
            .to_string();
        let mut cfg =
            prefill_router::PrefillRouterConfig::new(targets.clone(), &prefill.checkpoint);
        cfg.device.clone_from(&prefill.device);
        cfg.cache_dir.clone_from(&prefill.cache_dir);
        // Left at upstream's defaults when unset, rather than re-spelled here:
        // see `config::router::prefill`.
        if let Some(max_length) = prefill.max_length {
            cfg.max_length = max_length;
        }
        if let Some(batch_size) = prefill.batch_size {
            cfg.batch_size = batch_size;
        }
        let algorithm: Arc<dyn Algorithm> =
            Arc::new(
                cfg.build()
                    .map_err(|error| ConfigError::PrefillRouterLoad {
                        model: model.id.clone(),
                        message: error.to_string(),
                    })?,
            );
        by_model.insert(
            model.id.clone(),
            Entry {
                algorithm,
                targets,
                default_target,
            },
        );
    }
    Ok(by_model)
}

/// See [`super::decide`].
pub(super) async fn decide(
    by_model: &HashMap<String, Entry>,
    body: &Value,
    headers: &HeaderMap,
) -> Option<PrefillDecision> {
    // The same normalization `resolve_chain` matches on: a `[1m]` request must
    // reach the entry it resolves to, not miss the map and fall to the default.
    let model = strip_context_window_hint(body.get("model")?.as_str()?);
    let entry = by_model.get(model)?;
    let request = Request {
        llm_request: LlmRequest {
            model: Some(model.to_string()),
            messages: messages_from_body(body.get("messages").unwrap_or(&Value::Null)),
            ..Default::default()
        },
        raw_request: None,
        metadata: Some(metadata_from_headers(headers)),
    };
    let models = Arc::new(RuntimeModels::new(
        [(Category::Any, entry.targets.clone())].into(),
    ));
    // The upstream algorithm never calls `Driver::call_model` — it decides from
    // the transcript and its affinity alone — so `serve` is unreachable.
    // Erroring there is the honest arm: a future upstream rev that did offload
    // a call would fail loudly here rather than hang or answer from nothing.
    let outcome = drive(
        Arc::clone(&entry.algorithm),
        request,
        models,
        |_call| async {
            Err(LibsyError::AlgorithmError {
                message: "prefill_router makes no model calls".to_string(),
            })
        },
    )
    .await;
    match outcome {
        Ok(outcome) => match outcome.selected_model_ids.first() {
            // Defensive: upstream selects from the list it was built with, so
            // a target outside it cannot occur — but routing to an id that is
            // not configured would resolve through the default provider as a
            // literal upstream model name.
            Some(selected) if entry.targets.contains(selected) => Some(PrefillDecision {
                target: selected.to_string(),
                source: RouteSource::Prefill,
            }),
            other => {
                tracing::warn!(
                    model,
                    selected = ?other,
                    "prefill_router selected an unconfigured target; routing to the default target"
                );
                Some(fail_open(entry))
            }
        },
        Err(error) => {
            tracing::warn!(
                model,
                error = %error,
                "prefill_router drive failed; routing to the default target"
            );
            Some(fail_open(entry))
        }
    }
}

fn fail_open(entry: &Entry) -> PrefillDecision {
    PrefillDecision {
        target: entry.default_target.clone(),
        source: RouteSource::PrefillFailOpen,
    }
}

/// The correlation metadata libsy's affinity keys on.
///
/// It keys a root request on `session_id` and a child on `(session_id,
/// agent_id)` when `is_subagent`, so a delegated turn keeps its own decision
/// instead of replaying its parent's. A request carrying neither header falls
/// back to upstream's own message-hash affinity — deliberately left as
/// upstream's behaviour rather than overridden here.
///
/// Delegation is read through [`RouterContext::is_delegated`] and
/// [`RouterContext::pin_agent_id`], the same predicates the stage router pins
/// its turns with, rather than re-deriving it from the agent id here. The
/// class header is authoritative when sent, so a `main` turn that carries an
/// agent id is root traffic and must not be keyed as a child, and a
/// `subagent`/`workflow` turn is delegated whether or not it sent an id; only
/// when no class is sent — the default deployment, where the class is gated
/// off and the agent id is not — does a non-blank agent id decide.
fn metadata_from_headers(headers: &HeaderMap) -> Metadata {
    let hints = RouterContext::from_headers(headers);
    let text = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Metadata {
        session_id: text(hints.session_id),
        is_subagent: hints.is_delegated(),
        agent_id: text(hints.pin_agent_id()),
        ..Default::default()
    }
}

/// Translate an Anthropic `messages` array into libsy's neutral shape.
///
/// Two block kinds are the whole contract, because they are all the upstream
/// algorithm reads: it scores the latest non-empty **text** user turn, and its
/// affinity tells a human turn from a tool continuation by whether every block
/// is a `ToolResult`. An image, a `tool_use`, or a thinking block changes
/// neither answer, so carrying them would only cost allocations. A role that
/// is neither `user` nor `assistant` is skipped for the same reason.
pub fn messages_from_body(messages: &Value) -> Vec<Message> {
    let Some(messages) = messages.as_array() else {
        return Vec::new();
    };
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.get("role").and_then(Value::as_str)? {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return None,
            };
            Some(Message {
                role,
                content: content_blocks(message.get("content")?),
            })
        })
        .collect()
}

fn content_blocks(content: &Value) -> Vec<ContentBlock> {
    if let Some(text) = content.as_str() {
        return vec![ContentBlock::Text {
            text: text.to_string(),
        }];
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
            "text" => Some(ContentBlock::Text {
                text: block.get("text").and_then(Value::as_str)?.to_string(),
            }),
            "tool_result" => Some(ContentBlock::ToolResult(ToolResult {
                tool_call_id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                // The result's own payload is dropped with every other block
                // kind: the affinity asks only whether the turn *is* a
                // continuation, never what the tool returned.
                content: Vec::new(),
                is_error: block.get("is_error").and_then(Value::as_bool),
            })),
            _ => None,
        })
        .collect()
}
