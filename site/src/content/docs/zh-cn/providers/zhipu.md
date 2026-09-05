---
title: Zhipu (GLM China)
description: 用 ZHIPUAI_API_KEY 将 GLM Coding Plan 模型路由到智谱兼容 Anthropic 的 BigModel 端点。
---

**智谱**在中国 BigModel 平台上通过**兼容 Anthropic** 的端点提供 **GLM** 模型。内置的
`zhipu` 预设提供 `kind = "anthropic"`、
`base_url = "https://open.bigmodel.cn/api/anthropic"`，以及来自 `ZHIPUAI_API_KEY` 的
API 密钥认证。

国际版 Z.ai 端点见 [Z.ai (GLM)](/zh-cn/providers/zai/)。两者的主机和凭据互不相同。

## 配置上游

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # 让 Anthropic 作为无路由匹配模型(例如 claude-*)的默认项

[[upstreams]]
name = "zhipu"
provider = "zhipu"

[[routes]]
model = "glm-5.3"
provider = "zhipu"

[[routes]]
model = "glm-5.3-flash"
provider = "zhipu"
```

有序的 `[[upstreams]]` 会替换 shunt 的内置提供方，因此该配置必须声明它仍然回退到的
`anthropic` 默认项(`server.default_provider` 默认为 `anthropic`)。

## 凭据

```bash
export ZHIPUAI_API_KEY='...'
```

绝不要把密钥写进配置。`shunt check` 校验配置的结构，但不会读取密钥的值 —— 如果
`ZHIPUAI_API_KEY` 未设置，第一个被路由到 `zhipu` 的请求会返回认证错误。

## 模型

| 模型 ID | 说明 |
| :-- | :-- |
| `glm-5.3` | GLM Coding Plan 旗舰文本模型 |
| `glm-5.3-flash` | 更快的多模态档位；客户端可以添加 `[1m]`，shunt 会在路由匹配前移除它 |

在 Claude Code 中通过 `ANTHROPIC_MODEL`、`ANTHROPIC_CUSTOM_MODEL_OPTION` 或子 agent 的
`model:` frontmatter 选择一个已路由的 ID。要在 `/model` 选择器中展示条目,请用
`[models.upstream_model]` 映射声明一个以 `claude` 为前缀的别名 —— 见
[模型发现](/zh-cn/guides/model-discovery/)。

## 校验

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"glm-5.3-flash","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

确认响应的 `x-gateway-upstream` 头写的是 `zhipu`，然后
[将 Claude Code 指向 shunt](/zh-cn/guides/connect-claude-code/)。
