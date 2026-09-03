//! Gemini adapter implementation for Google Code Assist / Gemini endpoints.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{
        antigravity::{auth::inference_base_url, catalog::catalog_ids},
        resolve_credential, Credential,
    },
    config::AuthMode,
    model::antigravity_request::{
        antigravity_model_needs_catalog, antigravity_request_id, antigravity_session_id,
        antigravity_upstream_model_with, wrap_antigravity_envelope, AntigravityCatalog,
    },
    model::gemini::{map_gemini_error, GeminiSseMachine},
    model::gemini_request::{translate_request_for_model, wrap_code_assist_envelope},
    request::RequestBody,
    routing::Route,
    server::AppState,
};

pub struct GeminiAdapter;

impl Adapter for GeminiAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: RequestBody,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body).await })
    }
}

/// The Antigravity inference URL for a provider configured with `base_url`.
///
/// Split out so the production redirect is unit-testable without a live
/// request: `inference_base_url` carries the "production does not serve
/// Antigravity inference" rule that onboarding already applies.
fn antigravity_endpoint(base_url: &str, method: &str) -> String {
    let base_url = inference_base_url(base_url);
    format!("{base_url}/v1internal:{method}")
}

/// Carry the effort tier a `-tiered` catalog id does not name into the request
/// body, where the backend reads it.
///
/// Additive on purpose: `thinkingBudget` is already translated from the
/// client's `thinking` block and the backend accepts both fields together, so
/// the level joins the existing `thinkingConfig` rather than replacing it.
///
/// The one case it must not join is a budget of `0`, which is how
/// [`crate::model::gemini_request`] renders an explicitly *disabled* `thinking`
/// block. Writing a level there would send "do not think" and "think at
/// medium" in one object — and if the backend gives the level precedence, a
/// client that opted out of reasoning is billed for it anyway. A configured
/// `effort` is dropped here too, while a suffixed id (`…-flash-high`) would
/// still carry it in the id: an accepted asymmetry, because the client's
/// explicit opt-out is the stronger signal in the one place the backend can
/// honour it.
fn set_thinking_level(inner_req: &mut Value, level: &str) {
    if inner_req
        .pointer("/generationConfig/thinkingConfig/thinkingBudget")
        .and_then(Value::as_u64)
        == Some(0)
    {
        return;
    }
    let Some(object) = inner_req.as_object_mut() else {
        return;
    };
    let generation_config = object
        .entry("generationConfig")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(generation_config) = generation_config.as_object_mut() else {
        return;
    };
    let thinking_config = generation_config
        .entry("thinkingConfig")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(thinking_config) = thinking_config.as_object_mut() {
        thinking_config.insert(
            "thinkingLevel".to_string(),
            Value::String(level.to_string()),
        );
    }
}

fn append_gemini_events(line: &[u8], machine: &mut GeminiSseMachine, output: &mut Vec<u8>) {
    let Ok(line) = std::str::from_utf8(line) else {
        return;
    };
    let line = line.trim();
    let Some(json_str) = line.strip_prefix("data: ").map(str::trim) else {
        return;
    };
    if json_str.is_empty() || json_str == "[DONE]" {
        return;
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(json_str) {
        append_sse_events(machine.process_chunk(&parsed), output);
    }
}

fn append_sse_events(events: Vec<crate::model::gemini::SseEvent>, output: &mut Vec<u8>) {
    for event in events {
        let formatted = format!("event: {}\ndata: {}\n\n", event.event, event.data);
        output.extend_from_slice(formatted.as_bytes());
    }
}

async fn forward(
    state: AppState,
    route: Route,
    _uri: &Uri,
    _headers: &HeaderMap,
    body: RequestBody,
) -> Result<(StatusCode, Response<Body>), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| AdapterError {
            message: format!("unknown provider {}", route.provider),
            response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            failure: None,
        })?;

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;

    let (access_token, project_id) = match credential {
        Credential::GoogleOauth {
            access_token,
            project_id,
        }
        | Credential::AntigravityOauth {
            access_token,
            project_id,
        } => (access_token, project_id),
        Credential::ApiKey { value, .. } => (value, String::new()),
        _ => {
            return Err(AdapterError {
                message: "unsupported credential for Gemini adapter".to_string(),
                response: Box::new(StatusCode::UNAUTHORIZED.into_response()),
                failure: None,
            });
        }
    };

    let json_body = body.json();
    let is_streaming = json_body.get("stream").and_then(Value::as_bool) == Some(true);

    let mut inner_req = translate_request_for_model(json_body, &route.upstream_model)?;

    let base_url = provider.base_url.trim_end_matches('/');

    let method = if is_streaming {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    // Both subscription paths speak the Code Assist protocol: the same
    // `v1internal` methods under the `{model,project,request}` envelope. Only
    // the credential and the client identity differ, so the envelope is gated
    // on "is a subscription token", not on which provider issued it.
    let is_code_assist = matches!(
        provider.auth,
        AuthMode::GoogleOauth | AuthMode::AntigravityOauth
    );
    let (endpoint, payload) = if provider.auth == AuthMode::AntigravityOauth {
        // Antigravity speaks the same `v1internal` methods, but the client
        // identifies itself as the agent and names a session, and its catalog
        // ids carry their effort tier. See `wrap_antigravity_envelope` and
        // `antigravity_upstream_model`.
        //
        // The host is resolved rather than taken verbatim: production does not
        // serve Antigravity inference, and configs written against pre-0.40.0
        // docs pin it. Code Assist (`AuthMode::GoogleOauth`) genuinely lives
        // on production, so the redirect is scoped to this branch.
        let inference_base = inference_base_url(base_url);
        // Resolving twice is a no-op — the daily host resolves to itself — and
        // keeps the endpoint's own rule readable next to its unit tests.
        let endpoint = antigravity_endpoint(&inference_base, method);
        if !endpoint.starts_with(base_url) {
            tracing::debug!(
                provider = %route.provider,
                configured = %base_url,
                "antigravity base_url is pinned at the production Code Assist host, which does \
                 not serve inference; sending this request to the daily host instead"
            );
        }
        // The account's own catalog decides between the `-<tier>` and
        // `-tiered` forms of the same model; both exist in the wild and which
        // one an account is served changes over time. Discovery is cached and
        // fails open, so this costs at most one bounded request per account
        // per TTL and never fails the client's request. It is skipped outright for
        // an id no catalog could reshape — only Gemini ids carry a tier — so a
        // Claude- or GPT-routed Antigravity provider never pays for it.
        let catalog = if antigravity_model_needs_catalog(&route.upstream_model) {
            catalog_ids(
                &state.http_client,
                &inference_base,
                &access_token,
                &project_id,
            )
            .await
        } else {
            None
        };
        let model = antigravity_upstream_model_with(
            &route.upstream_model,
            route.effort.as_deref(),
            json_body,
            catalog.as_ref().map(|catalog| AntigravityCatalog {
                ids: &catalog.ids,
                fresh: catalog.fresh,
            }),
        );
        if let Some(level) = model.thinking_level {
            set_thinking_level(&mut inner_req, level);
        }
        let session_id = antigravity_session_id(&inner_req);
        let envelope = wrap_antigravity_envelope(
            &model.id,
            &project_id,
            inner_req,
            &antigravity_request_id(),
            &session_id,
        );
        (endpoint, envelope)
    } else if is_code_assist {
        let endpoint = format!("{base_url}/v1internal:{method}");
        let envelope = wrap_code_assist_envelope(&route.upstream_model, &project_id, inner_req);
        (endpoint, envelope)
    } else {
        let model_slug = &route.upstream_model;
        let endpoint = format!("{base_url}/v1beta/models/{model_slug}:{method}");
        (endpoint, inner_req)
    };

    let policy = provider.retry.policy();
    let http_client = state.http_client.clone();
    let payload_clone = payload.clone();
    let endpoint_clone = endpoint.clone();
    let is_google_oauth = is_code_assist;
    // Antigravity's backend is addressed as the Antigravity client; the Gemini
    // Code Assist path is not, so the fingerprint is per auth mode rather than
    // sent on every Code Assist request.
    let user_agent = (provider.auth == AuthMode::AntigravityOauth)
        .then(crate::auth::antigravity::version::user_agent);
    let token = access_token.clone();

    let ttfb_ms = state.config.server.timeouts.upstream_ttfb_ms;
    let response = crate::retry::send_with_retry(policy, &route.provider, || {
        let client = http_client.clone();
        let payload = payload_clone.clone();
        let endpoint = endpoint_clone.clone();
        let token = token.clone();
        let user_agent = user_agent.clone();
        async move {
            let mut req = client
                .post(&endpoint)
                .header("Content-Type", "application/json");

            if let Some(user_agent) = user_agent {
                req = req.header("User-Agent", user_agent);
            }

            if is_google_oauth {
                req = req.bearer_auth(&token);
            } else {
                req = req.header("x-goog-api-key", &token);
            }

            crate::upstream_timeout::wait(ttfb_ms, req.json(&payload).send()).await
        }
    })
    .await
    .map_err(|error| {
        error.into_adapter_error(|error| AdapterError {
            message: format!("network error calling Gemini backend: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            failure: None,
        })
    })?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|_| "failed to read error response".to_string());
        return Err(map_gemini_error(status, &body_text));
    }

    if is_streaming {
        let byte_stream = response.bytes_stream();
        let machine = GeminiSseMachine::new(&route.model);

        let sse_stream = futures_util::stream::unfold(
            (byte_stream, Vec::<u8>::new(), machine, false),
            |(mut bytes, mut line_buffer, mut machine, finished)| async move {
                if finished {
                    return None;
                }
                loop {
                    let mut sse_bytes = Vec::new();
                    while let Some(pos) = line_buffer.iter().position(|byte| *byte == b'\n') {
                        let line = line_buffer.drain(..=pos).collect::<Vec<_>>();
                        append_gemini_events(&line[..line.len() - 1], &mut machine, &mut sse_bytes);
                    }

                    if !sse_bytes.is_empty() {
                        return Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(sse_bytes)),
                            (bytes, line_buffer, machine, false),
                        ));
                    }

                    match bytes.next().await {
                        Some(Ok(chunk)) => line_buffer.extend_from_slice(&chunk),
                        Some(Err(error)) => {
                            return Some((
                                Err(std::io::Error::other(format!(
                                    "Gemini response stream failed: {error}"
                                ))),
                                (bytes, line_buffer, machine, true),
                            ));
                        }
                        None => {
                            let mut terminal_bytes = Vec::new();
                            if !line_buffer.is_empty() {
                                append_gemini_events(
                                    &line_buffer,
                                    &mut machine,
                                    &mut terminal_bytes,
                                );
                            }
                            let mut events = Vec::new();
                            machine.finish(&mut events);
                            append_sse_events(events, &mut terminal_bytes);
                            if terminal_bytes.is_empty() {
                                return None;
                            }
                            return Some((
                                Ok::<_, std::io::Error>(axum::body::Bytes::from(terminal_bytes)),
                                (bytes, Vec::new(), machine, true),
                            ));
                        }
                    }
                }
            },
        );

        let res_builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .header("Cache-Control", "no-cache");

        let response_res = res_builder
            .body(Body::from_stream(sse_stream))
            .map_err(|error| AdapterError {
                message: format!("failed to build response: {error}"),
                response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                failure: None,
            })?;

        Ok((StatusCode::OK, response_res))
    } else {
        let full_text = response.text().await.map_err(|error| AdapterError {
            message: format!("failed to read response body: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            failure: None,
        })?;

        let parsed = serde_json::from_str::<Value>(&full_text).map_err(|error| AdapterError {
            message: format!("invalid JSON from Gemini backend: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            failure: None,
        })?;
        let mut machine = GeminiSseMachine::new(&route.model);
        let _ = machine.process_chunk(&parsed);
        let final_json = machine.final_json();

        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let response_res = (StatusCode::OK, headers, axum::Json(final_json)).into_response();
        Ok((StatusCode::OK, response_res))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_production_pinned_antigravity_base_url_sends_inference_to_the_daily_host() {
        // Pre-0.40.0 docs told operators to write the production host, which
        // answers every Antigravity inference request with a fake 429. Such a
        // config still loads, so the request path is what has to redirect it.
        assert_eq!(
            antigravity_endpoint("https://cloudcode-pa.googleapis.com", "generateContent"),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
    }

    #[test]
    fn an_antigravity_base_url_in_front_of_the_backend_is_left_alone() {
        // A loopback proxy stands in front of the backend: redirecting past it
        // would egress straight around the endpoint the operator configured.
        assert_eq!(
            antigravity_endpoint("http://127.0.0.1:9999", "streamGenerateContent?alt=sse"),
            "http://127.0.0.1:9999/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn a_thinking_level_joins_the_translated_thinking_budget_rather_than_replacing_it() {
        // The `-tiered` catalog id names no effort, so the tier rides in
        // `thinkingLevel` — beside the `thinkingBudget` the client's own
        // `thinking` block already translated to. The backend accepts both.
        let mut inner_req = serde_json::json!({
            "contents": [],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 8192}}
        });

        set_thinking_level(&mut inner_req, "medium");

        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
    }

    #[test]
    fn an_explicitly_disabled_thinking_block_is_not_overridden_by_the_default_tier() {
        // `thinking: {"type": "disabled"}` translates to `thinkingBudget: 0`,
        // and the tier the resolver derived for it is the `medium` default
        // nobody asked for. Writing the level here would send "do not think"
        // and "think at medium" in one object, and bill reasoning to a client
        // that opted out.
        let mut inner_req = serde_json::json!({
            "contents": [],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}
        });

        set_thinking_level(&mut inner_req, "medium");

        assert!(
            inner_req["generationConfig"]["thinkingConfig"]
                .get("thinkingLevel")
                .is_none(),
            "{inner_req}"
        );
        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

    #[test]
    fn a_thinking_level_creates_the_config_objects_it_needs() {
        // A non-thinking request carries no `generationConfig` at all, so the
        // level would be dropped if the path were only ever patched in.
        let mut inner_req = serde_json::json!({"contents": []});

        set_thinking_level(&mut inner_req, "high");

        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert!(inner_req.get("contents").is_some());
    }

    #[test]
    fn complete_utf8_line_survives_arbitrary_byte_chunking() {
        let line = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Olá 🌊\"}]}}]}";
        let split = line.find('🌊').unwrap() + 1;
        let mut buffered = line.as_bytes()[..split].to_vec();
        buffered.extend_from_slice(&line.as_bytes()[split..]);
        let mut machine = GeminiSseMachine::new("gemini-test");
        let mut output = Vec::new();

        append_gemini_events(&buffered, &mut machine, &mut output);

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Olá 🌊"));
        assert!(!output.contains('�'));
    }

    #[test]
    fn unterminated_final_data_line_is_processed() {
        let mut machine = GeminiSseMachine::new("gemini-test");
        let mut output = Vec::new();

        append_gemini_events(
            br#"data: {"candidates":[{"content":{"parts":[{"text":"final"}]},"finishReason":"STOP"}]}"#,
            &mut machine,
            &mut output,
        );

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("final"));
        assert!(output.contains("event: message_stop"));
    }
}
