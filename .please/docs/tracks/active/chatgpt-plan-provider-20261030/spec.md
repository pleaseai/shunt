---
product_spec_domain: providers/chatgpt-plan
---

# ChatGPT Plan Provider (Sign in with ChatGPT)

> Track: chatgpt-plan-provider-20261030

## Overview

ChatGPT subscribers can let open-source tools spend their ChatGPT plan usage through OpenAI's
"Sign in with ChatGPT" (SIWC) token-sharing preview. A tool registers itself on the user's first
sign-in, receives an issued client id, and calls the public OpenAI Responses endpoint with the
user's plan-usage access token instead of an API key.

This track adds a `chatgpt-plan` provider to shunt so Claude Code users can divert mapped model
ids to their ChatGPT plan through that sanctioned path, distinct from the existing `codex`
provider (which reuses the Codex CLI's login against the ChatGPT backend). Users sign in one or
more named accounts from the CLI, shunt pools them, refreshes their tokens, keeps requests within
the preview's accepted parameters, and turns the preview's errors into the Anthropic error shape
Claude Code already understands.

The request-shape requirements below come from a live probe run on 2026-10-30
(`/tmp/siwc-probe/results.json`, summarized under Assumptions): shunt's existing OpenAI-dialect
translation of a Claude Code request with tools was accepted unchanged, and only parameters shunt
does not send for that dialect, or must never send, were rejected.

## Requirements

### Functional Requirements

- [ ] FR-1: offer a `chatgpt-plan` provider whose mapped model ids are served by the OpenAI Responses endpoint at `https://api.openai.com/v1/responses`, authenticated with a ChatGPT plan-usage access token
- [ ] FR-2: let a user sign in a named account from the CLI (`shunt login chatgpt-plan --name <account>`) through a browser authorization whose callback reaches a local listener on `127.0.0.1:1455`, and accept the callback URL pasted into the terminal as an alternative to the listener
- [ ] FR-3: register as a new agent client on an account's first sign-in, persist the issued client id with that account, and reuse it (without a new registration) on every later sign-in of the same account
- [ ] FR-4: create one stable, opaque host identifier per shunt installation before its first sign-in, persist it, and send it with every authorization request
- [ ] FR-5: generate a fresh state, nonce, and PKCE verifier per sign-in attempt, and accept a callback only when its state matches the pending attempt
- [ ] FR-6: validate the ID token of every sign-in against OpenAI's published signing keys — issuer, audience equal to the account's issued client id, expiry, and the attempt's nonce — tolerating a bounded clock skew between the host and OpenAI
- [ ] FR-7: store each account's validated identity, issued client id, tokens, granted scopes, and expiry in a credential record readable only by its owner, kept separate per account even when two accounts share an email address
- [ ] FR-8: renew an account's access token with its refresh token before expiry, no earlier than the server's `earliest_refresh_at`, sending the issued client id and the API resource, and persist the replacement refresh token
- [ ] FR-9: build the upstream request from shunt's existing OpenAI-dialect Responses translation, including plain function tools, `tool_choice`, `parallel_tool_calls`, `reasoning`, `text`, `include`, `max_output_tokens`, and `prompt_cache_key`
- [ ] FR-10: never send `temperature`, `metadata`, or `prompt_cache_retention` upstream, and always send `store: false` and `stream: true` — including when the Claude Code client asked for a non-streaming response
- [ ] FR-11: translate the preview's two upstream error shapes — a parameter-validation body `{"detail": "<message>"}` and a structured error with a `subscription_sharing_*` or `chatpass_v2_*` code, whether returned as an HTTP error or as a `response.failed` event mid-stream — into the Anthropic error shape with a matching error type and status
- [ ] FR-12: pool a provider's named accounts, select among them like the existing pooled providers, and when an account reports its plan usage limit, cool that account down and, if no output has reached the client yet, serve the request from another available account
- [ ] FR-13: let a user sign out a named account (`shunt logout chatgpt-plan --name <account>`): revoke its renewable session at OpenAI's revocation endpoint, clear its access, refresh, and ID tokens, and keep its issued client id and the host identifier for a later sign-in
- [ ] FR-14: reject at config validation a `chatgpt-plan` provider whose auth mode or upstream host is not the plan-usage combination, with a message naming the offending key
- [ ] FR-15: document the provider — setup, sign-in (including the paste fallback), sign-out, accepted models, preview limits, and error meanings — on every documentation surface a new provider requires, in English and its maintained translations

### Non-functional Requirements

- [ ] NFR-1: preserve streaming semantics end to end — upstream SSE events reach a streaming client as they arrive, with no buffering of the stream
- [ ] NFR-2: never print, log, or include in an error message any access token, refresh token, ID token, authorization code, or PKCE verifier
- [ ] NFR-3: leave the existing `codex` provider's behavior, credential files, and configuration unchanged

## Acceptance Criteria

- [ ] AC-001 (FR-2): When the user completes authorization in a browser that can reach `127.0.0.1:1455`, the system shall finish the sign-in from the received callback without further input.
- [ ] AC-002 (FR-2): When the user pastes the full callback URL into the waiting terminal, the system shall finish the sign-in from it even though the listener received nothing.
- [ ] AC-003 (FR-3): When an account that already has an issued client id signs in again, the system shall send that client id and no agent-name hint, and shall not create a new registration.
- [ ] AC-004 (FR-3): If a re-authorization callback carries a client id different from the account's saved one, then the system shall reject the sign-in and keep the saved credential unchanged.
- [ ] AC-005 (FR-3): If a first-sign-in callback carries no issued client id, then the system shall report the registration as incomplete and store no credential.
- [ ] AC-006 (FR-5): If a callback's state does not match the pending attempt, then the system shall refuse it without exchanging its code.
- [ ] AC-007 (FR-6): When the host clock lags OpenAI's by 5 seconds (the lag the 2026-10-30 probe observed), the system shall accept an otherwise valid ID token whose issued-at time is up to 5 seconds in the future.
- [ ] AC-008 (FR-6): If the ID token's nonce or audience does not match the attempt, then the system shall store no credential and tell the user the sign-in failed validation.
- [ ] AC-009 (FR-6): If the granted scopes lack `chatgpt.tokens.use.direct`, then the system shall tell the user plan usage was not granted and store no tokens for the account, keeping only its issued client id.
- [ ] AC-010 (FR-7): When two accounts with the same email address are signed in under different names, the system shall keep a separate credential record and issued client id for each.
- [ ] AC-011 (FR-8): While an account's access token is within its refresh window, the system shall renew it before forwarding the next request on that account.
- [ ] AC-012 (FR-8): If a refresh fails with a terminal grant error, then the system shall mark that account as needing sign-in and cool it down so healthy accounts serve requests first, as the existing pooled providers do.
- [ ] AC-013 (FR-9): When Claude Code sends a request with tools and extended thinking to a mapped model, the system shall forward the tools as plain function tools and return the model's tool call to Claude Code as an Anthropic `tool_use` block.
- [ ] AC-014 (FR-9): When the client sets `max_tokens`, the system shall forward it as `max_output_tokens`, and a response stopped by that cap shall reach Claude Code with stop reason `max_tokens`.
- [ ] AC-015 (FR-10): When the client request carries `temperature` or `metadata`, the system shall omit them from the upstream request.
- [ ] AC-016 (FR-10): When the Claude Code client requests a non-streaming response, the system shall still stream from upstream and return one complete Anthropic message.
- [ ] AC-017 (FR-11): When upstream returns HTTP 400 with body `{"detail": "Unsupported parameter: temperature"}`, the system shall return an Anthropic `invalid_request_error` carrying that message.
- [ ] AC-018 (FR-11): When upstream returns `subscription_sharing_unsupported_capability` with `param: tools`, the system shall return an Anthropic `invalid_request_error` naming the rejected parameter.
- [ ] AC-019 (FR-11): When a stream that has already begun ends with a `response.failed` event carrying `subscription_sharing_usage_limit_exceeded`, the system shall end the client stream with an Anthropic `rate_limit_error` event instead of a normal message stop.
- [ ] AC-020 (FR-11): When upstream returns `subscription_sharing_user_not_eligible`, `subscription_sharing_route_not_supported`, or a `chatpass_v2_*` authorization code, the system shall return an Anthropic `permission_error` and shall not retry the request or start a sign-in.
- [ ] AC-021 (FR-11): When upstream returns `subscription_sharing_usage_unavailable` or `subscription_sharing_user_unavailable`, the system shall keep the account's credentials and return an Anthropic `overloaded_error` once its bounded retries are spent.
- [ ] AC-022 (FR-11): When upstream returns `subscription_sharing_invalid_user`, the system shall force one token refresh and retry once, and if that also fails, mark the account as needing sign-in.
- [ ] AC-023 (FR-12): When one pooled account reports `subscription_sharing_usage_limit_exceeded` before any output was streamed, the system shall cool that account down and serve the same request from another available account.
- [ ] AC-024 (FR-12): If every pooled account is cooling down for its usage limit, then the system shall return an Anthropic `rate_limit_error` that points the user to ChatGPT usage settings.
- [ ] AC-025 (FR-13): When the revocation endpoint answers HTTP 200 (including for an already-invalid token), the system shall clear the account's tokens and report the sign-out as confirmed.
- [ ] AC-026 (FR-13): If revocation cannot be confirmed after bounded retries on network or 5xx failures, then the system shall still clear the tokens locally and tell the user remote revocation was not confirmed and can be finished in ChatGPT settings.
- [ ] AC-027 (FR-14): If a `chatgpt-plan` provider is configured with a different auth mode or a non-`api.openai.com` base URL, then `shunt check` shall fail and name the offending key.
- [ ] AC-028 (NFR-3): When a configuration that uses only the `codex` provider is loaded, the system shall behave exactly as before this change.

## Out of Scope

- Provisioning chatgpt-plan accounts from the opt-in admin web surface (CLI sign-in only in this track)
- Upstream model discovery: shunt serves the model ids mapped in config rather than listing the account's models from `GET /v1/models`
- Namespaced tools or `additional_tools` delivery — plain function tools are accepted, so no tool re-shaping is added
- Responses WebSocket transport and `previous_response_id` continuation for this provider
- New handling for hosted tools: `web_search` (which the preview accepts) passes through the existing translation unchanged, and `tool_search` (which the preview rejects) is never sent
- Transferring an existing session between hosts (self-hosted VM handoff)
- Any change to the `codex` provider or its credential files
- Account-picker UI beyond the CLI's `--name` selection

## Assumptions

- Live probe, 2026-10-30, model `gpt-6.1-sol`, token with scopes `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`:
  - Accepted (HTTP 200): the full captured shunt translation, with and without the `openai-beta: responses=experimental` header; plain function tools (`strict: false`), `tool_choice: "auto"`, `parallel_tool_calls`; `reasoning {effort, summary}`; `text {verbosity}`; `include ["reasoning.encrypted_content"]`; `max_output_tokens` (enforced — a cap of 16 ended with `incomplete: max_output_tokens`); `prompt_cache_key`; namespaced tools and an `additional_tools` input item.
  - Rejected: `temperature`, `metadata`, `prompt_cache_retention` → 400 `{"detail":"Unsupported parameter: …"}`; `store: true` → 400 "Store must be set to false"; `stream: false` → 400 "Stream must be set to true"; a `tool_search` tool → 400 `subscription_sharing_unsupported_capability`, `param: tools`.
  - The preview-limitations page lists `max_output_tokens` as unsupported; the probe shows it is accepted and enforced. This spec follows the observed behavior, and the plan should keep a single switch to stop forwarding it if OpenAI starts enforcing the page.
  - `GET /v1/models` with the token returned 200 and listed gpt-6.1-sol, gpt-6-astra, gpt-6.1-terra, gpt-6-sol, gpt-6-luna, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna.
  - The access token's audience is `https://api.openai.com/v1` and it carries no `chatgpt_account_id`, so no ChatGPT-account header is sent.
  - The container clock was about 5 seconds behind OpenAI's and made a strict ID-token issued-at check fail; FR-6's skew tolerance exists for this.
  - Responses carried no `x-request-id` header, so error reports cannot rely on one.
- Error-to-Anthropic mapping follows OpenAI's SIWC "Errors and recovery" table (status codes 400/401/403/429/503 per code); a code not in that table maps by its HTTP status like other providers' errors.
- The new login and logout commands write only new credential files under shunt's own directory; they do not change how any existing credential file is written back.
- The local callback listener binds `127.0.0.1` (not `localhost`) on port 1455, as the SIWC redirect registration requires; a host where that port is taken falls back to the paste path.
- Adding the `chatgpt-plan` provider kind and its auth mode adds public config values; no existing key changes meaning.
