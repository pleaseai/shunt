---
id: 005
title: "theagentrouter/agent-router: the renamed Envoy AI Gateway, a Go control plane over an Envoy data plane"
url: "https://github.com/theagentrouter/agent-router/tree/ce8f8069f609669a6eb1465a817f4996f4614bde"
date: 2026-09-16
summary: "Apache-2.0 Go project renamed from Envoy AI Gateway and donated to the Agentic AI Foundation, with every deployed identifier (CRD group, aigw CLI, Go module, images) deliberately unchanged; it is a control plane plus an ext_proc sidecar rather than a proxy. Translation is an explicit per-pair matrix of Go files behind one generic Translator interface, and auth is a closed six-value API-key/cloud-IAM enum with no subscription OAuth."
tags: [agent-router, envoy-ai-gateway, go, envoy, ext-proc, kubernetes, crd, llm-gateway, mcp, prior-art, shunt-comparison]
---

# theagentrouter/agent-router: the renamed Envoy AI Gateway, a Go control plane over an Envoy data plane

## Version examined

`theagentrouter/agent-router` `main` at commit [`ce8f8069f609669a6eb1465a817f4996f4614bde`](https://github.com/theagentrouter/agent-router/commit/ce8f8069f609669a6eb1465a817f4996f4614bde) ("fix: propagate MCPRoute OAuth issuer to SecurityPolicy (#2645)", committed 2026-09-15). Latest tagged release at the time of reading: `v1.1.0` (2026-08-21); release notes exist for `v0.6.0`, `v0.7.0`, `v1.0.0`, `v1.1.0`.

Repository metadata (GitHub API, 2026-09-16): Go, Apache-2.0, 2,094 stars, 374 forks, created 2024-10-21, described as "Manages Unified Access to Generative AI Services built on Envoy Gateway". Topics: `ai`, `ai-gateway`, `api-gateway`, `envoy`, `envoyproxy`, `inference`, `kubernetes`, `llm`.

## Direct answer

Agent Router is the **renamed Envoy AI Gateway**, now an Agentic AI Foundation (LF Projects) project. Its own tagline is the architecture in four words: **"Agent Router controls. Envoy carries."** Unlike agentgateway and shunt, it is *not* itself the proxy. The Go code is a control plane plus an ext_proc extension; the actual bytes move through Envoy.

The rename is deliberately **identifier-preserving** — the README's promise is "your manifests from yesterday apply tomorrow":

| Surface | Value (unchanged by the rename) |
|---|---|
| CRD API group | `aigateway.envoyproxy.io` |
| CRD kinds | `AIGatewayRoute`, `AIServiceBackend`, `BackendSecurityPolicy`, `MCPRoute`, `GatewayConfig`, `QuotaPolicy` |
| CLI | `aigw` |
| Namespace | `envoy-ai-gateway-system` |
| Go module | `github.com/envoyproxy/ai-gateway` (confirmed in `go.mod`; Go 1.27.1) |
| Images | `docker.io/envoyproxy/ai-gateway-*` |

Only the repository (`envoyproxy/ai-gateway` → `theagentrouter/agent-router`) and the website (`aigateway.envoyproxy.io` → `theagentrouter.ai`) moved, both with redirects. `GOALS.md` still carries its original "Envoy AI Gateway GOALS.md" heading.

## Architecture: three processes, not one

`cmd/` has exactly three binaries — `aigw`, `controller`, `extproc` — and that partition *is* the design:

1. **`controller`** — a controller-runtime manager that watches the CRDs and, through an **Envoy Gateway extension server** (`internal/extensionserver`), injects the AI-specific pieces into the xDS Envoy Gateway already generates.
2. **`extproc`** — a gRPC service implementing Envoy's `ext_proc` filter. This is where every AI-aware decision lives. `internal/extproc/processor.go` defines the contract, one instance per gRPC stream:

   ```go
   type Processor interface {
       ProcessRequestHeaders(context.Context, *corev3.HeaderMap)  (*extprocv3.ProcessingResponse, error)
       ProcessRequestBody(context.Context, *extprocv3.HttpBody)   (*extprocv3.ProcessingResponse, error)
       ProcessResponseHeaders(context.Context, *corev3.HeaderMap) (*extprocv3.ProcessingResponse, error)
       ProcessResponseBody(context.Context, *extprocv3.HttpBody)  (*extprocv3.ProcessingResponse, error)
       SetBackend(ctx context.Context, backend *filterapi.RuntimeBackend, routeName string, routerProcessor Processor) error
   }
   ```

   A processor runs at either the **router filter level** or the **upstream filter level**; `SetBackend` is the upstream-level hook and receives the router-level processor as its "parent" so route-time state carries into backend selection. A `passThroughProcessor` no-ops all five for non-AI traffic.
3. **`aigw`** — the CLI, and the standalone story.

### Standalone mode is the control plane wearing a disguise

`aigw run` does not bypass the Kubernetes machinery — it **fakes** it. `cmd/aigw/run.go` builds a `k8s.io/client-go/kubernetes/fake` clientset, runs the same controller translation against it, embeds an `envoy-gateway-config.yaml`, downloads and launches Envoy via `github.com/tetratelabs/func-e`, and wires extproc over a **Unix domain socket**. State and runtime files go to XDG paths (`$AIGW_STATE_HOME/runs/{runID}/`, `$AIGW_RUNTIME_DIR/{runID}/`).

```shell
OPENAI_API_KEY=sk-your-key aigw run
# then point any OpenAI-compatible client at http://localhost:1975/v1
```

So the laptop experience is one command, but the running system is still Envoy + a controller + an ext_proc sidecar. That is the sharpest contrast with shunt (single Rust binary, no sidecar, no Envoy) and with agentgateway (single Rust binary that *is* the proxy).

The README also defines a **two-tier gateway pattern**: a Tier One gateway for auth, top-level routing, and global rate limiting; a Tier Two gateway in front of a self-hosted model-serving cluster, with endpoint-picker support for inference optimisation.

## Translation: an explicit per-pair matrix

Where agentgateway resolves an ordered `InputFormat x ChatFormat` table at runtime, agent-router enumerates the matrix **as filenames** in `internal/translator/`, `<clientAPI>_<backend>.go`:

| Client API | Backends with a dedicated translator |
|---|---|
| OpenAI Chat Completions | `openai_openai`, `openai_azureopenai`, `openai_awsbedrock`, `openai_awsanthropic`, `openai_gcpvertexai`, `openai_gcpanthropic` |
| OpenAI Embeddings | `openai_embeddings`, `openai_azureopenai_embeddings`, `openai_awsbedrock_embeddings`, `openai_gcpvertexai_embeddings` |
| Anthropic Messages | `anthropic_anthropic`, `anthropic_openai`, `anthropic_awsbedrock`, `anthropic_awsanthropic`, `anthropic_gcpanthropic` |
| Count tokens | `counttokens_anthropic`, `counttokens_awsanthropic`, `counttokens_gcpanthropic`, `counttokens_openai_responses`, `counttokens_azureopenai_responses` |
| Tokenize | `tokenize_awsanthropic`, `tokenize_awsbedrock`, `tokenize_gcpanthropic`, `tokenize_gcpvertexai` |
| Other | `openai_responses`, `openai_completions`, `openai_speech`, `openai_transcription`, `imagegeneration_openai_openai`, `cohere_rerank_v2` |

Every file has a `_test.go` sibling. The cost of this shape is obvious (a new backend multiplies files); the benefit is that every pair is a named, individually testable unit with nowhere for a fallback to hide.

All of them satisfy one generic interface in `internal/translator/translator.go`:

```go
// This is created per request and is not thread-safe.
type Translator[ReqT any, SpanT any] interface {
    RequestBody(raw []byte, body *ReqT, flag bool) (newHeaders []internalapi.Header, mutatedBody []byte, err error)
    ResponseHeaders(headers map[string]string) (newHeaders []internalapi.Header, err error)
    ResponseBody(respHeaders map[string]string, body io.Reader, endOfStream bool, span SpanT) (
        newHeaders []internalapi.Header, mutatedBody []byte,
        tokenUsage metrics.TokenUsage, responseModel internalapi.ResponseModel, err error)
    ResponseError(respHeaders map[string]string, body io.Reader) (newHeaders []internalapi.Header, mutatedBody []byte, err error)
}
```

Three design notes worth carrying over:

- **`ResponseError` is a first-class method**, separate from `ResponseBody`. Non-2xx upstream bodies get their own translation path rather than falling through the success decoder. shunt keeps gateway-owned errors in the Anthropic shape (except on the inbound Codex endpoint, per AGENTS.md) — the same concern, solved here by giving the error path its own slot in the interface.
- **`ResponseBody` returns `tokenUsage` and `responseModel` alongside the mutated body**, so usage accounting and the "what model actually answered" signal are outputs of translation, not a second parse.
- **Optional capability interfaces** extend the base without widening it: `ContentTypeSetter` (multipart boundaries), `RequestHeadersSetter`, `HeaderValueFilterSetter` (allowlist/denylist filtering of individual values in a multi-valued request header — a documented no-op on unrecognised modes), `ResponseRedactor` / `AnthropicResponseRedactor` (produce a **copy** with sensitive fields redacted for debug logging; the original is never modified).

The type aliases pin the concrete request types: `OpenAIChatCompletionTranslator`, `OpenAIEmbeddingTranslator`, `OpenAICompletionTranslator`, `CohereRerankTranslator`, `AnthropicMessagesTranslator`.

## Supported providers

Per the README provider table: OpenAI, Azure OpenAI, Google Gemini, Vertex AI, AWS Bedrock, Mistral, Cohere, Groq, Together AI, DeepInfra, DeepSeek, Hunyuan, SambaNova, Grok, Tetrate Agent Router Service, Anthropic.

## Credentials: API keys and cloud IAM only

`BackendSecurityPolicy` enumerates exactly six types (`api/v1alpha1/backendsecurity_policy.go`), enforced mutually exclusive by CEL validation rules on the CRD:

`APIKey` · `AWSCredentials` · `AzureAPIKey` · `AzureCredentials` · `GCPCredentials` · `AnthropicAPIKey` (injected as `x-api-key`)

`internal/backendauth/` matches: `api_key.go`, `anthropicapikey.go`, `azureapikey.go`, `aws.go`, `azure.go`, `gcp.go`, `credential_override.go`. There is **no subscription-OAuth credential** — no Claude Code / ChatGPT / Codex login, no refresh-token rotation, no multi-account pool. Same conclusion as agentgateway, reached from a different direction (an explicit closed enum rather than an open provider schema).

## QuotaPolicy: CEL-costed token budgets

`api/v1alpha1/quota_policy.go` is the closest analogue to shunt's `[server.spend]` limits:

- Attaches to up to 16 `AIServiceBackend` refs.
- `CostExpression` is a **CEL expression** over request cost; the default is `total_tokens`, and the doc example is `input_tokens + cached_input_tokens + output_tokens` — i.e. cache-aware accounting is opt-in, spelled by the operator.
- `PerModelQuotas` (up to 128) override the service-wide quota per model, keyed on the model name **after** `ModelNameOverride` is applied.
- Exceeding a quota returns **429**.
- Conflict rule, stated in the CRD doc: when multiple QuotaPolicies define the same model for the same backend, **the policy whose namespace/name sorts alphabetically first wins**, and keys are sorted for deterministic snapshot generation.

Two `TODO`s in the same file are candid about the state: the rate-limit configuration is generated but "the descriptor set is not being set in the Envoy Configuration", and `ServiceQuota` enforcement awaits extension-server work plus an Envoy release supporting routing on non-exceeded quotas.

That alphabetical tie-break is the same class of hazard shunt hit when a derived `Ord` silently decided audit attribution — a deterministic-but-arbitrary rule that is correct only as long as it is documented and tested.

## MCP support

`MCPRoute` is a full CRD, not a config stanza. `internal/mcpproxy/` implements the proxy with `legacy.go` (SSE) and `modern.go` (Streamable HTTP) side by side, plus `session.go`, `crypto.go` (session encryption — `aigw run` exposes an iteration count for key derivation), `era.go`, `authorization.go`, and `fanout_subset.go`.

The `MCPRouteSpec` surface: `parentRefs`, `path`, `headers`, `hostnames`, `backendRefs`, `securityPolicy` (`oauth` / `apiKeyAuth` / `extAuth`), `backendTrafficPolicy`, `backendSelector`. Each `MCPRouteBackendRef` carries its own `path`, `securityPolicy` (API key from a `secretRef` or inline, delivered via a named header or query param), `forwardHeaders` (with optional rename to a different `backendHeader`), and a **`toolSelector`** — `include` / `includeRegex` / `exclude` / `excludeRegex` — so a route can expose a curated subset of an upstream server's tools.

`docs/proposals/` records the design trail: `001-ai-gateway-proposal`, `002-inference-gateway-support`, `003-epp-integration-proposal`, `004-vendor-specific-fields`, `005-original-model`, `006-mcp-gateway`, `007-batch-processing`, `008-gemini-context-caching`, `009-quota-aware-routing`, `010-mcp-token-exchange`, `011-mcp-backend-crd`, `012-mcp-new-spec-rc`, `012-mcp-tool-name-prefix-mode`.

## What this means for shunt

Ideas worth stealing:

1. **Give the error path its own method.** `ResponseError` as a sibling of `ResponseBody` structurally prevents an upstream error body from being decoded as a success body. shunt's error-shape rule (Anthropic shape everywhere except the inbound Codex endpoint) would be easier to keep honest with a dedicated translation entry point per adapter.
2. **Return usage and the answering model *from* translation.** Making `tokenUsage` and `responseModel` return values of `ResponseBody` means metrics can never disagree with what was actually parsed — no second pass, no drift.
3. **Optional capability interfaces over a widening trait.** `ContentTypeSetter`, `HeaderValueFilterSetter`, `ResponseRedactor` keep the core contract at four methods while letting individual pairs opt into extras. A Rust equivalent is a small set of optional traits rather than more `Option` fields on one trait.
4. **CEL cost expressions for spend.** Letting the operator write `input_tokens + cached_input_tokens + output_tokens` sidesteps the "whose definition of a token is this?" argument that any fixed formula invites — relevant to shunt's spend limits, where cache-token conventions differ per provider.
5. **Redaction that copies rather than mutates.** "The original response is never modified" is stated twice in the interface docs. shunt already has a redacting `Debug` for config secrets; the same discipline applies to response debug logging.

Where shunt differs fundamentally: agent-router **is not a proxy**. Adopting its ideas means adopting the interfaces, not the topology — shunt's single-binary, no-Envoy, no-sidecar shape is the opposite bet, and it is the right one for a gateway whose job is to sit in front of a developer's Claude Code or Codex CLI. Likewise its OpenAI-compatible-first front door and closed API-key/cloud-IAM credential enum leave shunt's actual niche — subscription OAuth login, refresh rotation, and quota-aware multi-account pooling — entirely uncontested.

## Related

- [agentgateway/agentgateway](./004-agentgatewayagentgateway-rust-llm-mcp-and-a2a-prox.md) — the other gateway surveyed in the same pass; Rust, and a proxy in its own right.
