use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ProtocolDescriptor {
    pub name: &'static str,
    pub version: &'static str,
    pub format: &'static str,
    pub spec_url: &'static str,
    pub endpoints: Vec<EndpointDescriptor>,
    pub request_headers: RequestHeaders,
    pub system_prompt_attribution: SystemPromptAttribution,
    pub model_discovery: ModelDiscovery,
}

#[derive(Debug, Serialize)]
pub struct EndpointDescriptor {
    pub method: &'static str,
    pub path: &'static str,
    pub description: &'static str,
    pub optional: bool,
}

#[derive(Debug, Serialize)]
pub struct RequestHeaders {
    pub forward_unchanged: Vec<HeaderDescriptor>,
    pub consumed: Vec<HeaderDescriptor>,
}

#[derive(Debug, Serialize)]
pub struct HeaderDescriptor {
    pub name: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SystemPromptAttribution {
    pub behavior: &'static str,
    pub disable_client_side: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ModelDiscovery {
    pub startup_request: &'static str,
    pub enable_client_side: &'static str,
    pub picker_filter: &'static str,
    pub non_claude_models: &'static str,
}

/// Describe the gateway-protocol contract implemented by this shunt version.
pub async fn get() -> Json<ProtocolDescriptor> {
    let descriptor = ProtocolDescriptor {
        name: "shunt",
        version: env!("CARGO_PKG_VERSION"),
        format: "anthropic-messages",
        spec_url: "https://code.claude.com/docs/en/llm-gateway-protocol",
        endpoints: vec![
            EndpointDescriptor {
                method: "POST",
                path: "/v1/messages",
                description: "Anthropic Messages inference, including SSE streaming responses",
                optional: false,
            },
            EndpointDescriptor {
                method: "POST",
                path: "/v1/messages/count_tokens",
                description: "Token counting; may pass through, estimate locally, or report unsupported depending on the route",
                optional: true,
            },
            EndpointDescriptor {
                method: "GET",
                path: "/v1/models",
                description: "Anthropic-compatible model discovery",
                optional: false,
            },
            EndpointDescriptor {
                method: "GET",
                path: "/routes",
                description: "Shunt-native configured route table",
                optional: false,
            },
            EndpointDescriptor {
                method: "GET",
                path: "/health",
                description: "Process liveness and package version",
                optional: false,
            },
            EndpointDescriptor {
                method: "GET",
                path: "/protocol",
                description: "This gateway-protocol descriptor",
                optional: false,
            },
        ],
        request_headers: RequestHeaders {
            forward_unchanged: vec![
                HeaderDescriptor {
                    name: "anthropic-version",
                    description: "Anthropic API version requested by the client",
                },
                HeaderDescriptor {
                    name: "anthropic-beta",
                    description: "Anthropic beta feature identifiers requested by the client",
                },
            ],
            consumed: vec![
                HeaderDescriptor {
                    name: "authorization",
                    description: "Client credential used or replaced according to the selected route authentication mode",
                },
                HeaderDescriptor {
                    name: "x-api-key",
                    description: "Client credential used or replaced according to the selected route authentication mode",
                },
                HeaderDescriptor {
                    name: "x-claude-code-session-id",
                    description: "Claude Code session identifier used for tracing, transport connection reuse, and the stage router's per-session tier pin",
                },
                HeaderDescriptor {
                    name: "x-claude-code-agent-id",
                    description: "Claude Code delegated-agent identifier; a turn carrying it pins its stage-router tier apart from the parent session's",
                },
                HeaderDescriptor {
                    name: "x-claude-code-parent-agent-id",
                    description: "Claude Code parent-agent metadata accepted by the gateway",
                },
                HeaderDescriptor {
                    name: "x-claude-code-request-class",
                    description: "Claude Code request class (main, subagent, workflow, compaction, auxiliary); subagent and workflow mark delegated work for the stage router, and the class wins over x-claude-code-agent-id when both are sent",
                },
                HeaderDescriptor {
                    name: "x-claude-code-agent-type",
                    description: "Claude Code agent type on delegated turns (teammate, a built-in agent id, or custom); the exact key the [models.subagents] by_type map routes delegated work on",
                },
                HeaderDescriptor {
                    name: "x-claude-code-context-compacted",
                    description: "Sent once on the first main turn after a compaction; the stage router escalates that turn and latches compacted onto the session pin until it expires",
                },
            ],
        },
        system_prompt_attribution: SystemPromptAttribution {
            behavior: "Shunt forwards the Claude Code attribution block unchanged and never strips, reorders, or merges it",
            disable_client_side: "Set CLAUDE_CODE_ATTRIBUTION_HEADER=0 in the Claude Code client",
        },
        model_discovery: ModelDiscovery {
            startup_request: "GET /v1/models?limit=1000",
            enable_client_side: "Set CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 in the Claude Code client",
            picker_filter: "Claude Code ignores discovered entries whose id does not begin with claude or anthropic",
            non_claude_models: "Use ANTHROPIC_CUSTOM_MODEL_OPTION for non-Claude model ids",
        },
    };

    tracing::info!("served GET /protocol descriptor");
    Json(descriptor)
}

#[cfg(test)]
mod tests {
    use super::get;

    #[tokio::test]
    async fn returns_gateway_protocol_descriptor() {
        let response = get().await;
        let body = serde_json::to_value(response.0).unwrap();

        assert_eq!(body["format"], "anthropic-messages");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        let endpoints = body["endpoints"].as_array().unwrap();
        assert!(endpoints
            .iter()
            .any(|endpoint| endpoint["path"] == "/protocol"));
        assert!(endpoints
            .iter()
            .any(|endpoint| endpoint["path"] == "/v1/messages"));
    }

    /// The descriptor names every `x-claude-code-*` header the router reads.
    /// `compaction` and `prev-tool-durations` are deliberately absent: nothing
    /// consumes them yet, and a header listed as consumed must be one the
    /// gateway actually reads.
    #[tokio::test]
    async fn lists_the_router_hint_headers_as_consumed() {
        let response = get().await;
        let body = serde_json::to_value(response.0).unwrap();
        let consumed: Vec<&str> = body["request_headers"]["consumed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|header| header["name"].as_str().unwrap())
            .collect();

        for name in [
            crate::routing::context::SESSION_ID_HEADER,
            crate::routing::context::AGENT_ID_HEADER,
            crate::routing::context::REQUEST_CLASS_HEADER,
            crate::routing::context::AGENT_TYPE_HEADER,
            crate::routing::context::CONTEXT_COMPACTED_HEADER,
        ] {
            assert!(
                consumed.contains(&name),
                "{name} must be listed as consumed"
            );
        }
        assert!(
            !consumed.contains(&"x-claude-code-compaction")
                && !consumed.contains(&"x-claude-code-prev-tool-durations"),
            "a header nothing reads must not be listed as consumed"
        );
    }
}
