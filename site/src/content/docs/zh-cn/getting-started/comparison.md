---
title: 对比
description: shunt 与其他 Claude Code 网关和 LLM 代理的对比 —— 同类项目分组、功能矩阵、优势与刻意的范围边界。
---

一份有依据的对比,比较 shunt 与它最接近的工具。目的是把 shunt 的设计边界讲明白:它刻意*不做*什么,以及范围内真正的改进机会在哪里。

:::note[范围]
关于 shunt 的论断引用 [shunt 仓库](https://github.com/pleaseai/shunt)中的 `file:line`。关于 CLIProxyAPI 的论断已对照 `router-for-me/CLIProxyAPI@main` 验证。关于通用网关(LiteLLM/Portkey/bifrost)的论断保持在各项目自己宣传的层面,与 shunt 自己 README 的"Related work"定位一致。
:::

## 1. shunt 是什么(以及不是什么)

shunt 是一个**符合规范的 Claude Code LLM 网关**:它实现 Claude Code 官方的 `ANTHROPIC_BASE_URL` 网关契约(`/v1/messages`、`/v1/models` 发现、归属/头部透传),并做**按 `model` id 的选择性**分流 —— 主会话留在 Claude 上,只把你点名的模型转到另一个提供方(ChatGPT/Codex、OpenAI、xAI)。对映射的模型它在 Anthropic Messages ⇄ OpenAI Responses API 之间转换,其余一切原样透传给 Anthropic。路由纯粹按请求的 `model` id 进行 —— 没有提示词形状指纹识别(`README.md:104-131`)。

这个焦点是下面所有对比围绕的轴线。shunt 为**转换保真度与 Claude Code 原生行为**优化,配有一个把感知模型的主动配额轮换与被动故障转移结合起来的 Anthropic OAuth 账户池,而不是面向广泛多租户的机群运营。

## 2. 同类项目分组

| 分组 | 例子 | 与 shunt 的关系 |
|---|---|---|
| **订阅支撑的 CC 代理(同一类)** | **raine/claude-code-proxy** | **总体上最接近的同类** —— Rust 单二进制,按 `model` 路由,Codex WebSocket + `previous_response_id` 续传,4 个订阅 OAuth 后端(Codex/Kimi/Grok/Cursor) |
| **带 Codex OAuth 的宽泛 Claude Code 代理** | **CLIProxyAPI**(router-for-me) | 最接近的*宽泛*同类 —— 在 Codex/ChatGPT OAuth、Codex WebSocket v2、工具转换上重叠 |
| **窄的 Claude Code → Codex 替换** | insightflo/chatgpt-codex-proxy | 同样的推理层替换,单一后端 |
| **通用 Claude Code 路由器** | musistudio/claude-code-router、1rgs/claude-code-proxy、fuergaosi233/claude-code-proxy | 通常是*全局*模型替换,而非按模型分流 |
| **通用 AI 网关** | LiteLLM、Portkey、bifrost、modelgate | 相邻基础设施 —— 可作*后端*,但不是 Claude Code 原生 |

## 3. 功能矩阵

图例:● 完整 · ◐ 部分 / 有变通 · ○ 无 · — 设计上不适用

| 能力 | shunt | raine/ccp | CLIProxyAPI | 通用 CC 路由器 | 通用网关 |
|---|:--:|:--:|:--:|:--:|:--:|
| Claude Code 网关协议合规(`/v1/models` 发现、归属透传) | ● | ◐ | ◐ | ◐ | ○ |
| 按 `model` id 选择性分流,主会话留在 Claude(非全局替换) | ● | ◐³ | ◐ | ○ | ◐ |
| Anthropic Messages ⇄ OpenAI Responses 转换 | ● | ● | ● | ◐(多为 chat-completions) | ◐(chat-completions) |
| ChatGPT/Codex **订阅**(OAuth)后端 | ● | ●⁴ | ● | 少见 | ○ |
| Codex **WebSocket** Responses 传输 | ● | ● | ● | ○ | ○ |
| **转换路径上的**上传裁剪(`previous_response_id` 续传) | ● | ● | ○(仅透传) | ○ | ○ |
| tool-search / `defer_loading` / `tool_reference` 处理 | ◐(垫片:可用,无上下文节省;原生可选启用⁸) | ○⁵ | ◐(上游)/ ●(分叉) | ○ | ○ |
| reasoning 往返到 Claude Code `thinking` | ●(加密) | ◐(Kimi/Grok;**Codex 被丢弃**) | ◐ | ○ | ◐ |
| 多账户负载均衡 / 故障转移 | ◐⁷ | ○ | ● | 部分 | ● |
| 后端广度 | 4 个提供方¹ | 4 个订阅⁶ | 11 个后端² | 不一 | 100–1600+ |
| 管理 API / 仪表盘 | ◐(可选启用的管理界面) | ◐(监控 TUI) | ● | 部分 | ● |
| 用量 / 配额 / 成本跟踪 | ○(仅 Sentry 指标) | ○ | ● | 部分 | ● |
| 插件 / 拦截器系统 | ○ | ○ | ● | 部分 | ● |
| 语言 / 体积 | Rust,1 个二进制 | Rust,1 个二进制 | Go | Node/Python | Go/Node/Python |
| 配置模型 | TOML + env,热重载 | env + 配置文件 | YAML + 管理 API | 不一 | YAML/UI |

¹ shunt:两种适配器*类型*(`anthropic` 透传、`responses` 转换)加 4 个内置提供方(Anthropic、OpenAI、ChatGPT/Codex、xAI) —— 任何 Anthropic-Messages 或 OpenAI-Responses 端点都只需配置即可(`src/config.rs:180-190,316-363`)。
² CLIProxyAPI:aistudio、antigravity、claude、codex、codex-ws、gemini、gemini-vertex、kimi、openai-compat、xai、xai-ws。
³ raine/ccp 和 shunt 一样按 `ANTHROPIC_MODEL` 逐模型路由,但**没有 Anthropic 透传适配器** —— 未知的 model id 返回 400,因此无法在只分流点名模型的同时把主会话留在 Claude 上。
⁴ raine/ccp 实现了**自己的** ChatGPT OAuth(PKCE 浏览器 + 设备码登录);shunt 复用 Codex CLI 登录(`~/.codex/auth.json`),自己的 PKCE 流程是未完成的 TODO(`src/auth/mod.rs:18-19`)。
⁵ **通过阅读 raine/ccp 源码确认**(`fe80a6b`,2026-07-11):不存在任何 tool-search 处理(`defer_loading` / `tool_reference` / `tool_search` / `advanced-tool-use` 零匹配)。工具被按白名单重建为 `{name, description, parameters}`(`src/providers/codex/translate/request.rs:476-494`),所以 `defer_loading:true` 被静默丢弃 —— 不会 400,但也不省上下文;ToolSearch 结果中的 `tool_reference` 块渲染为 `[unsupported content block omitted: tool_reference]`(`request.rs:836-842`),而不是 shunt 干净的 `"Loaded tool: X"`。因此是 ○(相对 shunt 的 ◐):对 raine/ccp 强行开启 `ENABLE_TOOL_SEARCH` 会把发现循环的结果劣化成占位符。默认情况下 Claude Code 自己的门控会在非第一方 base URL 后面关闭 tool search,所以这个问题保持潜伏。
⁶ raine/ccp 订阅后端:Codex(ChatGPT Plus/Pro)、Kimi(kimi.com)、Grok(grok.com)、Cursor Agent —— 全部经由订阅 OAuth。
⁷ shunt 只为 Anthropic `claude_oauth` 池化显式账户:会话粘性选择、按提供方轮询、基于每账户 5h/7d 配额头部的感知模型主动轮换、冷却、401 后强制刷新,以及对配额拒绝的 429 和 5xx 响应的被动故障转移。ChatGPT/Codex 仍是单账户;每账户用量报告未实现。
⁸ **[#82]** 增加了一个可选启用的按提供方 `tool_search` 标志(`src/config.rs:250-261,1041-1049`),把 Claude Code 的工具搜索映射到 OpenAI Responses API 自己的原生、客户端执行的 `tool_search` 协议 —— `ToolSearch` → `tool_search`,其 `tool_use` → `tool_search_call`,`tool_reference` → 一个以结构化 JSON 携带已加载工具完整 schema 的 `tool_search_output` 项(`src/model/responses_request.rs`) —— 而不是把 schema 折叠进文本。默认关闭:仅当原版 OpenAI 或 ChatGPT/Codex Responses 风味路由到 gpt-5.4+ 模型时适用,并且在实时探测确认给定后端接受 shunt 发出的形状之前,一直由该标志把关。xAI/Grok 路由和 gpt-5.2 及以下的模型无论标志如何都保留 #43 垫片。

> "raine/ccp" = [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy)。

## 4. shunt 领先之处

- **Claude Code 原生保真度。** shunt 实现*官方*网关契约,而不是老一代 CC 代理用的"对子代理系统提示词做哈希"启发式;会话留在 Claude Code 的运行框架内(同样的工具循环、技能、脚本路径) —— 只有 token 生成被外包(`README.md:97-131`)。多数通用路由器和网关以 OpenAI chat-completions 为中心,不尊重 Claude Code 的发现/归属面。

- ***转换*路径上的上传裁剪。** 因为 shunt 在 Anthropic ⇄ Responses 之间转换(Claude Code 从不发送 `previous_response_id`),它*合成*续传:把会话记录存在池化连接上,用类型感知的规范化将下一个请求与之做差分,再注入 `previous_response_id` + 输入增量 —— 在 Claude→Codex 路径上实现真正的上传裁剪(`src/adapters/codex_continuation.rs:79-114`)。这**并非**独有:**raine/claude-code-proxy 做同一类事情**(可选启用的 `CCP_CODEX_PREVIOUS_RESPONSE_ID`,按会话键控,只追加)。这两个 Rust 订阅代理共享它 —— 真正的反差在于 **CLIProxyAPI** 这类**透传**代理:它的 Codex WS 不存会话记录/response-id,依赖 Codex CLI 客户端发送 `previous_response_id`,因此在*它的*转换路径上每一轮都重发全部输入(另有一个工具输出"修复"缓存来保持 tool-call 配对一致)。

- **规范化深度 + reasoning 保真度(相对最近的同类)。** 在共享续传的这对之中,shunt 在两个轴上比 raine/claude-code-proxy 走得更远:(1) 它的续传规范化会解析 `function_call.arguments` 并往返 reasoning 的 `encrypted_content`/signature,因此在只比形状就会中断的工具轮次之间,续传仍持续触发(`src/adapters/codex_continuation.rs:11-48`);(2) 它**把 Codex reasoning 作为 `thinking` 转发给 Claude Code**,而 raine/claude-code-proxy **完全丢弃 Codex reasoning 块**(其 README 将此列为限制)。任何未预见的形状仍会回退到全量输入 —— 绝不会有错误的上下文,只会错过一次优化。

- **小而可审计的体积。** 单个 Rust 二进制,TOML+env 配置带安全失败(fail-closed)的启动校验和热重载;没有需要保护的运行时插件面。

## 5. shunt 落后之处 —— 以及原因

大多数差距是**刻意的范围边界**,不是疏忽。shunt 自己的 README 把通用网关(LiteLLM/Portkey/bifrost)定位为*相邻基础设施 / 可能的后端*,而非同一种产品。

- **Anthropic OAuth 多账户刻意保持狭窄。** shunt 为 `auth = "claude_oauth"` 提供主动加被动的账户池:`x-claude-code-session-id` 粘性、按提供方轮询、在 5 小时或起决定作用的周桶撞墙之前的感知模型轮换、账户冷却、401 后凭据文件强制刷新,以及配额拒绝的 429 或 5xx 响应后的故障转移([详情](/zh-cn/guides/anthropic-multi-account/))。它**不**池化 ChatGPT/Codex 账户,不对刚切换的账户做并发爬坡,也不暴露每账户用量。CLIProxyAPI、LiteLLM 和 Portkey 提供更广的机群式均衡与可见性;其余差距见 §6 的 G–H 项。
- **后端广度窄。** 只有 Anthropic-Messages 透传或 OpenAI-Responses 转换;除非暴露这两种协议之一,否则没有原生的 Gemini/Bedrock/Azure/Ollama。
- **没有完整的管理 API / 用量-配额 / 成本跟踪。** 可选启用的[管理 Web 界面](/zh-cn/guides/admin-remote-provisioning/)覆盖 `claude_oauth` 与 `chatgpt_oauth` provider 的浏览器账户预配和只读账户池状态。Claude 的配额 header 会填充使用率,而 Codex 没有此类 header,所以这些单元格为空。仍然没有通用管理 API、按请求的用量核算或成本跟踪;可观测性只有可选启用的 Sentry 指标(`src/metrics.rs`)。完整的 HTTP 暴露面见 [HTTP 端点](/zh-cn/reference/endpoints/)。CLIProxyAPI 提供完整的管理 API + 配额/用量管理器和第三方仪表盘生态;即便同类的 raine/claude-code-proxy 也内置了 shunt 没有对应物的**监控 TUI**(实时会话、活跃/近期请求、错误事件)。
- **没有自己的 ChatGPT OAuth 登录。** shunt 复用 Codex CLI 登录(`~/.codex/auth.json`);第一方 PKCE 流程是未完成的 TODO(`src/auth/mod.rs:18-19`)。raine/claude-code-proxy 在此是先例 —— 它自带 `codex auth login`(PKCE)**和** `codex auth device`(设备码),因此无需安装 Codex CLI 也能工作。
- **没有插件 / 拦截器系统。** 适配器集合是固定的双变体 `match`(`src/proxy.rs:152-163`);CLIProxyAPI 有完整的插件宿主(RPC ABI、认证提供方、执行器路由、请求/响应转换器)。
- **只有纯 HTTP**(TLS 不在范围内,`docs/m4-inbound-auth.md:13`)。

## 6. 改进机会(来自本对比)

按与 shunt 使命的契合度排序。**范围内**的条目推进高保真转换 / Claude Code 原生行为;**范围边界**的条目会把 shunt 推向机群网关,应先做有意识的决定。

### 范围内

- **A. tool-search 上下文节省(已跟踪:[#43])。** shunt 把 `tool_reference` 渲染为只有名字的 `"Loaded tool: X"` 文本,并预先转发*全部*延迟工具 schema(`src/model/responses_request.rs:393-403,475-508`) —— 循环可用,但默认收不回任何上下文。移植服务端模拟(过滤延迟+未加载的工具,在 `tool_reference` 时注入完整 schema) —— 参考实现:CLIProxyAPI PR #1892(`Adamcf123/CLIProxyAPI@main`)。**[#82] 已部分解决**:一个可选启用的按提供方 `tool_search = true` 标志,现在把工具搜索映射到 Responses API 原生、客户端执行的 `tool_search` 协议,而不是文本垫片,适用于原版 OpenAI 或 ChatGPT/Codex 提供方路由到 gpt-5.4+ 模型(见上文脚注 8)。在对后端接受度做实时探测之前默认关闭,所以在运营者选择启用之前,垫片(以及 xAI/Grok 和旧模型的零节省缺口)仍是基线。

- **B. Codex WS:实时探测续传规范化(已跟踪:[#45])。** Reasoning/`function_call` 规范化已对照 3 个来源做过 schema 校验,但尚未实时探测(`docs/m7-codex-websocket.md:250-270`)。任何未纳入的字段都会静默落入安全的全量输入回退 —— 正确性无虞,但是*潜在错过的优化*。一轮探测能确认续传是否按应有的频率触发。

- **C. Codex WS:流中途失败的回退(已解决:[#46])。** 两处修复共同解决了它。**[#93]** 在签出(checkout)阶段移除了一个*成因*:半开(half-open)的池化套接字现在无法通过复用存活探测(要求及时返回 `Pong`,而不仅是本地写入成功),并在发送本轮帧之前被替换为一次全新握手,因此陈旧连接不再会在流中途断开。随后 **[#46]** 堵上了签出探测无法覆盖的残余窗口(帧发送*之后*、首个事件之前套接字死亡的 send→first-event 窗口):`open_ws_turn` 会先探读(peek)首个事件,`commit_or_fallback` 在首个事件之前的传输错误时将本轮改由 HTTP 重新驱动(`src/adapters/responses/mod.rs`)。首个事件已流式发出*之后*的失败才是真正的流中途失败——重启会重复已发出的输出——因此以干净的 Anthropic `error` SSE 事件浮出,而不是重放;通过 `previous_response_id` 的轮次中途续传是刻意的非目标(部分输出已提交给客户端)。已由 `tests/codex_websocket_fallback.rs` 覆盖。

- **D. Codex WS:投机预热(`generate:false`)(已跟踪:[#47])。** 今天明确不在范围内(`docs/m7-codex-websocket.md:53-58`),但它是真实的 Codex 延迟优化 —— 在第一个 token 之前预热套接字/上下文。等续传经过实时探测后值得重新审视。

- **E. 上游重试/退避(已跟踪:[#48])。** M4 计划的有界重试/退避未实现(`docs/implementation-plan.md:247`);瞬时的上游 429/5xx 错误直接浮出。一个小而幂等的重试能在不扩大范围的前提下提升韧性。

- **F. 文档漂移:`GET /protocol`(已跟踪:[#49])。** README 宣传在 `GET /protocol` 有机器可读的规范(`README.md:110`),但 `src/server.rs` 里没有这条路由。实现它(成本低,而且是网关协议故事的一部分)或者更正文档。

### 范围边界(做之前先决定)

- **G. ChatGPT/Codex 的最小多账户。** 完整的负载均衡不在范围内,但重度用户会撞上 ChatGPT/Codex 的滚动窗口上限,此时在少数几个 `~/.codex/auth.json` 式登录之间做 *fill-first* 轮换(烧完一个账户的窗口再换下一个)的价值不成比例地高。这是相对 CLIProxyAPI 的最大单项功能差距,也是最值得设计讨论的一项。

- **H. 每账户配额/用量可见性。** 承接 G —— 一旦多个订阅账户投入使用,展示每个账户的 5h/7d 窗口(如 CLIProxyAPI 生态所做)就变得有用。与可观测性缺口相关联。

- **I. 原生 Gemini(及其他)后端。** 只有当 shunt 越过 Anthropic-Messages / OpenAI-Responses 二元结构时才相关。目前不在范围内。

## 7. 一句话总结

shunt 处于光谱中**高保真、Claude Code 原生**的一端。它最近的同类是 **raine/claude-code-proxy** —— 同一类(Rust、订阅 OAuth、按 `model` 路由、Codex WS + `previous_response_id` 续传) —— 相对它,shunt 的优势是更深的续传规范化、Codex reasoning 保真度(raine 丢弃它)、Anthropic 透传路径(把主会话留在 Claude)以及 xAI OAuth;raine 的优势是内置监控 TUI、第一方 ChatGPT OAuth 登录和 Kimi/Cursor 的广度。相对 **CLIProxyAPI**,shunt 在转换路径的上传裁剪上占优(CLIProxyAPI 的 WS 是透传),并按设计舍弃了大部分机群功能(广泛的多账户负载均衡、完整的管理 API、插件、后端广度)。它现在提供一个带感知模型的主动配额调度加被动故障转移的窄 Anthropic OAuth 账户池,但 ChatGPT/Codex 的池化仍是刻意留下的缺口。范围内价值最高的工作是完成 tool-search 上下文节省([#43]) —— 已由 Codex/OpenAI 上可选启用的原生 `tool_search` 路径([#82])部分解决。此后 Codex WS 传输的首个事件之前 HTTP 回退缺口已被填补([#46]);续传规范化实时探测([#45])仍未完成。最值得权衡的刻意缺口是 ChatGPT/Codex 的最小 fill-first 多账户。

[#43]: https://github.com/pleaseai/shunt/issues/43
[#82]: https://github.com/pleaseai/shunt/issues/82
[#45]: https://github.com/pleaseai/shunt/issues/45
[#46]: https://github.com/pleaseai/shunt/issues/46
[#47]: https://github.com/pleaseai/shunt/issues/47
[#48]: https://github.com/pleaseai/shunt/issues/48
[#49]: https://github.com/pleaseai/shunt/issues/49
[#93]: https://github.com/pleaseai/shunt/issues/93
