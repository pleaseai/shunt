---
title: "xAI Grok Provider"
description: "Route Claude Code to xAI Grok with an API key or a SuperGrok / X Premium+ subscription OAuth login."
---

## Overview

shunt ships a built-in `xai` provider that translates Anthropic Messages to xAI's OpenAI-Responses-shaped API at `https://api.x.ai/v1/responses`. It is reachable two ways: an **API key** (`XAI_API_KEY`, the default) or a **subscription OAuth** login that reuses a SuperGrok / X Premium+ plan with no separate API billing [src/config.rs:300-311](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L300-L311) [docs/m6-xai-provider.md](https://github.com/chatbot-pf/shunt/blob/main/docs/m6-xai-provider.md).

| Path | Credential | Cost model | Setup |
|---|---|---|---|
| API key (default) | `XAI_API_KEY` env var | Pay-per-token xAI API billing | `export XAI_API_KEY=...` + routes |
| Subscription OAuth | `~/.shunt/xai-auth.json`, written by `shunt login xai` | Included in SuperGrok / X Premium+ | `auth = "xai_oauth"` + one device-code login |

## API key setup

The built-in provider already defines `kind = "responses"`, `base_url = "https://api.x.ai/v1"`, and `api_key_env = "XAI_API_KEY"`, so a minimal `shunt.toml` only adds routes [src/config.rs:295-311](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L295-L311):

```toml
[[routes]]
model = "grok-build-0.1"    # flagship coding model
provider = "xai"

[[routes]]
model = "grok-4.3"
provider = "xai"
```

```bash
export XAI_API_KEY=xai-...
shunt run
```

## Subscription OAuth setup

Flip the provider's auth mode and log in once with the RFC 8628 device-code flow:

```toml
[providers.xai]
auth = "xai_oauth"
```

```bash
shunt login xai   # prints a verification URL + short code; approve in any browser
```

`shunt login xai` requests a device code from `https://auth.x.ai/oauth2/device/code`, prints the verification URL, and polls the token endpoint until the login is approved [src/auth/xai_login.rs:52-100](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_login.rs#L52-L100). Because there is no loopback callback server, the flow works over SSH, in containers, and on headless VPS hosts. Credentials are written atomically at `0600` to `~/.shunt/xai-auth.json` (override with `SHUNT_XAI_AUTH_FILE`) [src/auth/mod.rs:119-129](https://github.com/chatbot-pf/shunt/blob/main/src/auth/mod.rs#L119-L129).

## Token lifecycle

| Behavior | Detail | Source |
|---|---|---|
| Expiry | Access-token JWT `exp` claim, 5-minute buffer | [src/auth/xai_auth.rs:87-121](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_auth.rs#L87-L121) |
| Refresh-token rotation | xAI rotates the refresh token on every refresh; shunt persists the rotated pair | [src/auth/xai_auth.rs:106-120](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_auth.rs#L106-L120) |
| Concurrent refresh | Process-wide single-flight mutex; waiters re-read the winner's rotated pair | [src/auth/xai_auth.rs:43-48](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_auth.rs#L43-L48) |
| Refresh `403` | Subscription tier is not entitled to API access — re-login will not help; the error points at the `XAI_API_KEY` path | [src/auth/xai_auth.rs:233-246](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_auth.rs#L233-L246) |
| Refresh `400`/`401` | Consumed or invalid refresh token — run `shunt login xai` again | [src/auth/xai_auth.rs:233-246](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_auth.rs#L233-L246) |

## Safety and request shaping

- **Bearer-leak guard:** a provider with `auth = "xai_oauth"` must be `kind = "responses"`, use an https `base_url`, and stay on an `x.ai` host; anything else fails validation at boot, so the subscription bearer can never be sent off-origin or over plaintext [src/config.rs:424-435](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L424-L435).
- **Reasoning is opt-in:** several grok models reject `reasoning.effort` with a 400, so shunt sends a `reasoning` object only when an `effort` is configured on the route or provider [src/model/responses_request.rs:34-51](https://github.com/chatbot-pf/shunt/blob/main/src/model/responses_request.rs#L34-L51).
- The xAI dialect also omits the `text` object and the `OpenAI-Beta` header, and always sends `store: false`; detection is table-driven from the provider's auth mode and base-URL host, never a hardcoded provider name [src/config.rs:489-505](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L489-L505) [src/adapters/responses.rs:214-240](https://github.com/chatbot-pf/shunt/blob/main/src/adapters/responses.rs#L214-L240).

## Related Pages

| Page | Relation |
|---|---|
| [Configuration](./configuration.md) | Provider table keys and route syntax |
| [Authentication](../02-deep-dive/authentication.md) | All auth modes side by side |
| [Adapters and Translation](../02-deep-dive/adapters-and-translation.md) | Responses translation internals |

## Sources

- [src/auth/xai_auth.rs](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_auth.rs)
- [src/auth/xai_login.rs](https://github.com/chatbot-pf/shunt/blob/main/src/auth/xai_login.rs)
- [src/config.rs:295-311](https://github.com/chatbot-pf/shunt/blob/main/src/config.rs#L295-L311)
- [docs/m6-xai-provider.md](https://github.com/chatbot-pf/shunt/blob/main/docs/m6-xai-provider.md)
- [docs/running.md](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md)
