---
title: HTTP Endpoints
description: The endpoints shunt serves as a Claude Code LLM gateway.
---

| Method | Path | Purpose |
| :-- | :-- | :-- |
| `HEAD` | `/` | Liveness probe |
| `GET` | `/` | Human-readable landing (version + endpoint list) |
| `GET` | `/health` | Healthcheck — `{"status":"ok","version":"x.y.z"}` |
| `GET` | `/v1/models` | [Model discovery](/guides/model-discovery/) — returns your `[[models]]` entries |
| `POST` | `/v1/messages` | Inference — routed per the request's `model` id |
| `POST` | `/v1/messages/count_tokens` | [Token counting](/guides/effort-and-context/#token-counting-count_tokens) |

`GET /` and `GET /health` stay open even when [`[server.auth]`](/guides/shared-gateway/) is enabled (healthcheck tools usually cannot attach tokens) and expose nothing sensitive — only status, version, and the already-public endpoint list.

## Gateway protocol

shunt implements the official [Claude Code LLM gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol): correct header and body-field forwarding, feature pass-through, and system-prompt attribution handling. Gateway-owned errors are returned in the Anthropic error shape, and streaming responses are relayed without buffering (with optional [keepalive pings](/guides/shared-gateway/#sse-keepalive-pings)).
