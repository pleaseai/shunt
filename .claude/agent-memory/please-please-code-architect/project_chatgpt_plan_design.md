---
name: chatgpt-plan-provider-design
description: Minimal-change design decisions for the chatgpt-plan (Sign in with ChatGPT) track in shunt, 2026-10-30
metadata:
  type: project
---

Track `.please/docs/tracks/completed/chatgpt-plan-provider-20261030/`. Simplicity-perspective design chose:

- New `AuthMode::ChatgptPlanOauth` + `StoreFamily::ChatgptPlan` + `Credential::ChatgptPlanOauth { access_token }`; no new `ResponsesFlavor` (api.openai.com host already yields `OpenAi`, which is what the preview accepts). Preset only, NOT in `Config::default()` / `upstream_order` (NFR-3: default routing must not change).
- Pool reuses the xAI fork points in `adapters/responses/pool.rs` (`resolve_or_cooldown`, `classify_first`, `classify_retry`, `force_refresh_or_cooldown`); new code lives in `pool_plan.rs` (credential resolve/refresh, body shaping, relay and cooldown helpers) and `pool_plan_peek.rs` (the pre-content peek) because pool.rs is already >2000 lines. pool.rs keeps only the call sites: `resolve_plan_or_cooldown` / `force_refresh_plan_or_cooldown` in the xAI-style fork points, `peek_if_plan` in `classify_first` / `classify_retry`, and `relay_events` / `cool_on_relayed_limit` in the streaming and non-streaming relay loops.
- Pre-output in-stream `response.failed` rotation: byte-level peek at the start of a 2xx upstream response (hold only the lifecycle frames response.created/in_progress/queued, up to a 64 KiB limit, replay via `reqwest::Body::wrap_stream`, precedent `adapters/error_body.rs`; both `response.failed` and `error` events are checked); a SIWC failure is converted to a synthetic HTTP-status response so existing `classify_codex` rotate/refresh logic applies unchanged. Works for streaming and non-streaming loops. The pool's streaming path may already have sent the client its synthetic `message_start`, but no model output has reached the client at that point, so rotating is still invisible to it.
- ID token validation reuses `auth::inbound_jwt::verify` with a per-attempt `JwtIssuerRule` (`require` nonce, identity_claim `sub`, leeway 60s).
- Host id lives at `<accounts dir>/.host-id` (ignored by `scan_account_dir`); cross-process flock on `<file>.lock` mirrors Antigravity (`auth/antigravity/auth.rs:157`).
- Protocol facts (authorize params, `ext_agent_host_id=urn:uuid:...`, `agent_name_hint`, redirect `/auth/callback`, `earliest_refresh_at` is epoch seconds, expires_in 3600) are in `/tmp/siwc-probe/signin.py` (ephemeral).

**Why:** user asked for the smallest diff reusing existing machinery.
**How to apply:** if another perspective/revision of this design is requested, start from these choices and re-verify file:line references first.
