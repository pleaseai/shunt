# ChatGPT plan provider: Sign in with ChatGPT probe findings

Observed behavior of OpenAI's "Sign in with ChatGPT" (SIWC) token-sharing preview on `api.openai.com/v1/responses`, measured on 2026-10-30 and used to shape the `chatgpt-plan` provider ([m19](../m19-chatgpt-plan-provider.md), [ADR-0007](../../.please/docs/decisions/0007-chatgpt-plan-provider.md)). This note records what the preview did, not what its documentation says. It holds no token material: no access, refresh, or ID token, no authorization code, no PKCE verifier, no real client id, and no account email.

Re-run the probe before relying on any row below after OpenAI changes the preview.

## Setup of the probe

- Model `gpt-6.1-sol`, one signed-in ChatGPT account, token scopes `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`.
- The request was shunt's own OpenAI-dialect translation of a captured Claude Code turn with tools.
- The access token's audience is `https://api.openai.com/v1`. It carries no `chatgpt_account_id`, so shunt sends no ChatGPT-account header.

## Authorization and tokens

| Item | Observed |
| :-- | :-- |
| Issuer | `https://auth.openai.com` |
| Authorization endpoint | `<issuer>/api/accounts/authorize` |
| Token endpoint | `<issuer>/api/accounts/oauth/token` |
| Revocation endpoint | `<issuer>/api/accounts/oauth/revoke`, read from `/.well-known/openid-configuration`. Public client, form POST of `token`, `token_type_hint=refresh_token`, `client_id`. HTTP 200 means revoked, including for a token that was already invalid. |
| First sign-in | `client_id=dynamic_agent_client` plus `agent_name_hint=shunt`. The callback carries the issued id, which has the form `oaiapp_…`. |
| Later sign-ins | The saved issued client id and no hint. |
| Redirect | `http://127.0.0.1:1455/auth/callback`. The registration requires the literal address and port. |
| Other authorize parameters | `ext_agent_host_id` (a stable per-installation id), `response_type=code`, `resource=https://api.openai.com/v1`, `state`, `nonce`, S256 PKCE challenge. |
| Token and refresh bodies | Carry the issued `client_id` and `resource`. |
| Token response | `access_token`, `refresh_token`, `id_token`, `expires_in` (3600), `scope`, `earliest_refresh_at` (Unix seconds). |
| ID token | RS256 with a `kid`. `aud` is a JSON array that contains the issued client id. No `azp`. `exp - iat` is 3600. `nonce` is present. |
| Clock | The probe container's clock ran about 5 seconds behind OpenAI's. A strict `iat` check failed; shunt tolerates a bounded skew. |

## Request shape

| Request field | Result |
| :-- | :-- |
| Full shunt translation, with or without `openai-beta: responses=experimental` | Accepted (HTTP 200) |
| Plain function tools (`strict: false`), `tool_choice: "auto"`, `parallel_tool_calls` | Accepted |
| `reasoning {effort, summary}`, `text {verbosity}`, `include ["reasoning.encrypted_content"]` | Accepted |
| `prompt_cache_key` | Accepted |
| `max_output_tokens` | Accepted and enforced. A cap of 16 ended with `incomplete: max_output_tokens`. OpenAI's preview-limitations page lists the field as unsupported; shunt follows the observed behavior behind one code constant. |
| Namespaced tools, an `additional_tools` input item | Accepted (shunt does not send them) |
| `temperature`, `metadata`, `prompt_cache_retention` | Rejected: 400 `{"detail": "Unsupported parameter: <name>"}` |
| `store: true` | Rejected: 400, "Store must be set to false" |
| `stream: false` | Rejected: 400, "Stream must be set to true" |
| A `tool_search` tool | Rejected: 400 `subscription_sharing_unsupported_capability`, `param: tools` |
| `GET /v1/models` | 200. Listed gpt-6.1-sol, gpt-6-astra, gpt-6.1-terra, gpt-6-sol, gpt-6-luna, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna. |

Responses carried no `x-request-id` header, so an error report cannot rely on one.

## Error codes

| Code | HTTP status | Anthropic error type shunt returns |
| :-- | :-- | :-- |
| `subscription_sharing_usage_limit_exceeded` | 429 | `rate_limit_error` |
| `subscription_sharing_usage_unavailable` | 503 | `overloaded_error` |
| `subscription_sharing_user_unavailable` | 503 | `overloaded_error` |
| `subscription_sharing_unsupported_capability` | 400 | `invalid_request_error` |
| `subscription_sharing_user_not_eligible` | 403 | `permission_error` |
| `subscription_sharing_route_not_supported` | 403 | `permission_error` |
| `subscription_sharing_invalid_user` | 401 | `authentication_error` |
| `chatpass_v2_scope_not_authorized`, `chatpass_v2_invalid_authorization_context` (any `chatpass_v2_*`) | 403 | `permission_error` |
| Parameter rejection `{"detail": "…"}` | 400 | `invalid_request_error` |

The same codes arrive in two shapes: as an HTTP error, or as a `response.failed` (or `error`) event on a `200 OK` stream. The in-stream shape is what makes pre-content rotation necessary; see [m19 section 6](../m19-chatgpt-plan-provider.md#6-failover).
