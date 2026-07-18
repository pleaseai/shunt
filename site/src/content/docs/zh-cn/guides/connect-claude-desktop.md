---
title: 连接 Claude Desktop
description: 将 Claude Desktop 的第三方推理指向 shunt、配置认证并选择模型。
---

基于官方的 [使用 LLM 网关部署 Claude Desktop](https://claude.com/docs/third-party/claude-desktop/gateway) 指南 —— shunt *就是*你要指向的那个网关。shunt 实现了 Anthropic [Messages API](https://docs.claude.com/en/api/messages)（支持流式传输和工具使用的 `POST /v1/messages`）以及可选的 `GET /v1/models`，这正是 Claude Desktop 第三方推理所期望的网关契约。

:::note[Claude Desktop 的网关配置由管理员管理，而不是用户文件]
下面的每个键都是一项 [MDM / Bootstrap 托管设置](https://claude.com/docs/third-party/claude-desktop/mdm)。请在应用内窗口（**Developer → Configure Third-Party Inference…**）中设置它们，该窗口会导出 `.mobileconfig`（macOS）或 `.reg`（Windows）文件；也可以通过 MDM 推送它们。这里没有类似 `~/.claude` 的用户文件。
:::

## 1. 将 Claude Desktop 指向 shunt

在 **Developer → Configure Third-Party Inference…** 中，将 **Inference provider** 设置为 **Gateway**，并将 **Gateway base URL** 设置为正在运行的 shunt（默认绑定 `127.0.0.1:3001`）：

| Claude Desktop 键 | 值 |
| :-- | :-- |
| `inferenceProvider` | `gateway` |
| `inferenceGatewayBaseUrl` | `http://127.0.0.1:3001`（或你的公开 shunt URL） |

shunt 提供纯 HTTP；除回环部署外，都应像[共享网关](/zh-cn/guides/shared-gateway/)一样，在前置服务中终止 TLS 或通过隧道访问。

## 2. 选择认证方式

Claude Desktop 提供三种方式。shunt 可以直接配合静态密钥（以及它的凭据 helper 变体）；按用户 SSO 是网关侧的能力，shunt **不支持**这种入站认证。

| Claude Desktop 方式 | shunt 侧 | 说明 |
| :-- | :-- | :-- |
| **静态 API 密钥**（`inferenceGatewayApiKey`） | [`[server.auth]`](/zh-cn/guides/shared-gateway/) 客户端 token | 推荐。 |
| **凭据 helper**（`inferenceCredentialHelper`） | 输出 `[server.auth]` 客户端 token 的可执行文件 | 适用于已经签发网关凭据的组织。 |
| **交互式 SSO**（`inferenceGatewayOidc` + `inferenceCredentialKind: interactive`） | 不支持入站 | shunt 验证*静态* token，而不是外部 IdP JWT —— 见下文。 |

### 静态 API 密钥（推荐）

在 shunt 上启用 [`[server.auth]`](/zh-cn/guides/shared-gateway/#入站客户端-token)，并为每位用户分配一个客户端 token：

```toml
[server.auth]
header = "x-shunt-token"          # 默认
tokens_env = "SHUNT_CLIENT_TOKENS"
```

将该 token 填入 Claude Desktop 的 `inferenceGatewayApiKey`。shunt 接受通过 `Authorization: Bearer` 或 `x-api-key` 传入的客户端 token，因此两种 **Gateway auth scheme** 都可以使用：

| Claude Desktop 键 | 值 |
| :-- | :-- |
| `inferenceGatewayApiKey` | 你的 shunt 客户端 token |
| `inferenceGatewayAuthScheme` | `bearer`（默认）或 `x-api-key` |

未配置 `[server.auth]` 时，shunt 不要求入站凭据（适合个人回环网关）；Claude Desktop 仍要求填写该字段，因此可以输入任意占位值。

该 token 会门控 `GET /v1/models` 和注入凭据的模型（映射/池化模型）；[透传模型](/zh-cn/guides/connect-claude-code/#3-提供映射提供方的凭据)保持开放，并携带运营者自己的提供方凭据。

:::caution[shunt 未实现 Claude Desktop 的 SSO 契约]
Claude Desktop 的**交互式登录**（`inferenceGatewayOidc`）会让应用通过外部 IdP（Entra、Okta 等）认证，并把该 IdP 的 JWT 发送给网关；网关必须验证 `iss`/`aud`。shunt 没有入站 JWT 验证器 —— 它的 [`[server.gateway]`](/zh-cn/guides/gateway-login/) OAuth 界面是**为 Claude Code 构建的设备流登录**，两者采用不同的契约。若要在 Claude Desktop 中按用户进行 SSO 归属，请在 shunt 前部署能验证 JWT 的代理（LiteLLM、Kong、Envoy），或分发按用户设置的静态 token。
:::

## 3. 选择模型

shunt 提供 `GET /v1/models`，因此 Claude Desktop 会在启动时自动发现并填充模型选择器。显示哪些模型由以下两项因素决定。

**发现过滤器。** Claude Desktop 的自动发现只显示*可识别为 Claude* 的 id —— 即 tier 命名的 id（`claude-sonnet-*`、`claude-opus-*`、`claude-haiku-*`、`claude-fable-*`）。shunt 的内置目录与参考 Claude apps gateway 完全一致 —— 其中有九个 tier 命名的 id，因此 Claude Desktop 会显示全部模型：

```json
// GET /v1/models — 内置目录（auto_include_builtin_models），全部采用 tier 命名
{ "data": [
  { "id": "claude-opus-4-6" },   { "id": "claude-sonnet-4-5-20250929" },
  { "id": "claude-haiku-4-5-20251001" }, { "id": "claude-fable-5" },
  { "id": "claude-opus-4-8" },   { "id": "claude-opus-4-7" },
  { "id": "claude-opus-4-1-20250805" },  { "id": "claude-sonnet-5" },
  { "id": "claude-sonnet-4-6" }
] }
```

维护的 `claude-<slug>-via-<provider>` 别名（可用于 Claude Code 的模式）会**被 Claude Desktop 丢弃** —— 见[模型发现 → Claude Desktop 只识别 tier 命名的 id](/zh-cn/guides/model-discovery/#claude-desktop-只识别-tier-命名的-id)。

**公开非 Anthropic 后端。** 有两种方式：

- **映射一个 tier 命名的 id**，通过 `[[routes]]` 的 `upstream_model` 让在 Desktop 中选择该 id 时解析到你的后端：

  ```toml
  [[routes]]
  model = "claude-sonnet-5"        # Claude Desktop 识别的 tier 命名 id
  provider = "codex"
  upstream_model = "gpt-5.6-sol"   # 真实后端 slug
  ```

- **在 Desktop 侧覆盖发现**，通过显式的 `inferenceModels` 列表填写 shunt 实际路由的确切 id。如果每个条目都是完整 id，Claude Desktop 会跳过 `/v1/models` 请求。

:::note[尚未输出 `anthropic_family_tier`]
如果 `/v1/models` 条目携带 `anthropic_family_tier` 字段（例如 `sonnet` 这样的 tier 名称），Claude Desktop 也会接受一个*不透明*别名。shunt 目前不输出该字段（[#211](https://github.com/pleaseai/shunt/issues/211)），因此当前要在 Desktop 中公开后端，只能使用 tier 命名的 id 或显式的 `inferenceModels` 列表。
:::

## 4. 验证

确认 shunt 能使用客户端 token 响应发现和推理请求：

```bash
# 发现 —— 设置 [server.auth] 后由 token 门控
curl -s "$SHUNT_URL/v1/models" -H "Authorization: Bearer $SHUNT_CLIENT_TOKEN" | jq '.data[].id'

# 你映射的 tier 命名 id -> 分流到后端
curl -s -X POST "$SHUNT_URL/v1/messages" \
  -H "Authorization: Bearer $SHUNT_CLIENT_TOKEN" \
  -H "anthropic-version: 2023-06-01" -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

然后打开 Claude Desktop；模型选择器中应列出 tier 命名的条目。如果选择器为空，说明发现结果已被过滤掉（id 不是 tier 命名）或无法访问 `/v1/models` —— 请显式设置 `inferenceModels` 作为后备方案。
