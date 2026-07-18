# Changelog

## [0.23.0](https://github.com/pleaseai/shunt/compare/v0.22.0...v0.23.0) (2026-07-18)


### Features

* **admin:** add OIDC browser login to the admin surface ([#205](https://github.com/pleaseai/shunt/issues/205)) ([f87a4ee](https://github.com/pleaseai/shunt/commit/f87a4ee816d578817bfea4a8081a4972af125b58))
* **codex:** 분석 이벤트 수신 후 폐기 싱크 추가 ([#204](https://github.com/pleaseai/shunt/issues/204)) ([f4e0e8b](https://github.com/pleaseai/shunt/commit/f4e0e8b92609850a0eec3ccb3af92c8946306143))
* **gateway:** add OIDC approval provider for device login ([#202](https://github.com/pleaseai/shunt/issues/202)) ([d0d0202](https://github.com/pleaseai/shunt/commit/d0d02020486616b3ea21b724288725d8424b4a0f))
* **gateway:** M-B — per-user GET /managed/settings with ETag/304 and telemetry env push ([#199](https://github.com/pleaseai/shunt/issues/199)) ([9503cea](https://github.com/pleaseai/shunt/commit/9503cea0eb4fad3dd49988b6844c1bf103da89ec))

## [0.22.0](https://github.com/pleaseai/shunt/compare/v0.21.0...v0.22.0) (2026-07-18)


### Features

* **gateway:** persist gateway-login OAuth sessions across restarts ([#197](https://github.com/pleaseai/shunt/issues/197)) ([dfbdd8c](https://github.com/pleaseai/shunt/commit/dfbdd8ca6c5ffe22cd2cff9c28a9149b67bb60d9))
* **pool:** quota-aware Codex scheduling and storm control ([#198](https://github.com/pleaseai/shunt/issues/198)) ([fb8868f](https://github.com/pleaseai/shunt/commit/fb8868f2fcfeddf14cdbed17c8fa723207eb03a7))

## [0.21.0](https://github.com/pleaseai/shunt/compare/v0.20.0...v0.21.0) (2026-07-18)


### Features

* **gateway:** OAuth device-flow login with pluggable approval provider ([#192](https://github.com/pleaseai/shunt/issues/192)) ([6f7164c](https://github.com/pleaseai/shunt/commit/6f7164cab18a7ec2a133a89a3df5c940df9044f9))
* **metrics:** add streaming and pool observability ([#191](https://github.com/pleaseai/shunt/issues/191)) ([d8ad836](https://github.com/pleaseai/shunt/commit/d8ad83654f90ac7e9c0acc6557843ff6d93a984e))

## [0.20.0](https://github.com/pleaseai/shunt/compare/v0.19.1...v0.20.0) (2026-07-16)


### Features

* **pool:** persist account quota state across restarts ([#185](https://github.com/pleaseai/shunt/issues/185)) ([d392bcb](https://github.com/pleaseai/shunt/commit/d392bcb0b758560fe04f0163f6cabbaa4f7b9b67))

## [0.19.1](https://github.com/pleaseai/shunt/compare/v0.19.0...v0.19.1) (2026-07-16)


### Bug Fixes

* **cursor:** restore cursor:* on the current AgentService wire (HTTP/2 + tools, images, modes) ([#177](https://github.com/pleaseai/shunt/issues/177)) ([2039c27](https://github.com/pleaseai/shunt/commit/2039c274d43ef7d02f6b30830db76b48348d8f30))


### Performance Improvements

* request-path hot-path optimizations — Tier 3–4 ([#149](https://github.com/pleaseai/shunt/issues/149)) ([#182](https://github.com/pleaseai/shunt/issues/182)) ([914f4dd](https://github.com/pleaseai/shunt/commit/914f4dd43221ecfe7def21b9c38afb6c9704ddca))

## [0.19.0](https://github.com/pleaseai/shunt/compare/v0.18.1...v0.19.0) (2026-07-16)


### Features

* **codex:** surface per-account usage in the admin pool dashboard ([#179](https://github.com/pleaseai/shunt/issues/179)) ([b3cd018](https://github.com/pleaseai/shunt/commit/b3cd018c23bb6b17399977ac44ef2c657950666c))
* **pool:** coalesce accounts by upstream identity ([#178](https://github.com/pleaseai/shunt/issues/178)) ([c9d500f](https://github.com/pleaseai/shunt/commit/c9d500fda07c2e66e19150f3a7828be7485a0a12))
* **usage:** add authenticated client usage endpoint ([#175](https://github.com/pleaseai/shunt/issues/175)) ([e903bda](https://github.com/pleaseai/shunt/commit/e903bda106cbaf1f21f27da93e0e35c2e6bdf782))


### Bug Fixes

* **anthropic:** preserve route alias in relayed responses ([#174](https://github.com/pleaseai/shunt/issues/174)) ([2313985](https://github.com/pleaseai/shunt/commit/2313985201e07a6a561a3bef5b84873ed1a350f4))

## [0.18.1](https://github.com/pleaseai/shunt/compare/v0.18.0...v0.18.1) (2026-07-15)


### Performance Improvements

* **codex:** bound WebSocket event channel ([#167](https://github.com/pleaseai/shunt/issues/167)) ([1c65f7b](https://github.com/pleaseai/shunt/commit/1c65f7b8edfea3804a6fb77bd158378541691e02))
* **codex:** single-flight CodexAuthStore refresh ([#168](https://github.com/pleaseai/shunt/issues/168)) ([6079053](https://github.com/pleaseai/shunt/commit/60790530d3771cefb1ca4133f4f2ba487d85d021))

## [0.18.0](https://github.com/pleaseai/shunt/compare/v0.17.0...v0.18.0) (2026-07-15)


### Features

* **codex:** OpenAI-shaped error envelopes for gateway-owned errors on the inbound Codex endpoint ([#146](https://github.com/pleaseai/shunt/issues/146)) ([3de1bc4](https://github.com/pleaseai/shunt/commit/3de1bc4a3372486f894e97d7660460f6a6bf4819))


### Performance Improvements

* **auth:** cache account pool store scans ([#163](https://github.com/pleaseai/shunt/issues/163)) ([b42b1d4](https://github.com/pleaseai/shunt/commit/b42b1d454fae9226dfef5a44ad4639d4d7a18cb8))
* **cursor:** reduce SSE delta allocations ([#166](https://github.com/pleaseai/shunt/issues/166)) ([e84e378](https://github.com/pleaseai/shunt/commit/e84e37860c1d159037bcc087014676fb206d3aa9))
* **request-body:** avoid redundant parse/serialize/clone on the hot path ([#161](https://github.com/pleaseai/shunt/issues/161)) ([32bbeb0](https://github.com/pleaseai/shunt/commit/32bbeb0e22ce72194d0e41c2f89df62e4e1b181b))
* **responses:** avoid front-draining SSE frames ([#164](https://github.com/pleaseai/shunt/issues/164)) ([14e905d](https://github.com/pleaseai/shunt/commit/14e905d78ba4a50a9b8e2c0c0bfd2d52fc1177d4)), closes [#152](https://github.com/pleaseai/shunt/issues/152)
* **responses:** skip content accumulation while streaming ([#165](https://github.com/pleaseai/shunt/issues/165)) ([6ec8a22](https://github.com/pleaseai/shunt/commit/6ec8a220fc8fe09d41683895b83d66824ed6fe88))

## [0.17.0](https://github.com/pleaseai/shunt/compare/v0.16.0...v0.17.0) (2026-07-15)


### Features

* **admin:** add Codex account provisioning + pool view to admin web ([#144](https://github.com/pleaseai/shunt/issues/144)) ([be8a55a](https://github.com/pleaseai/shunt/commit/be8a55a22f95b0778752ee77906a3d997a15693c))

## [0.16.0](https://github.com/pleaseai/shunt/compare/v0.15.0...v0.16.0) (2026-07-14)


### Features

* **auth:** add refreshable Claude OAuth login (--mode oauth + admin web) ([#142](https://github.com/pleaseai/shunt/issues/142)) ([a4f49b7](https://github.com/pleaseai/shunt/commit/a4f49b79d1dd96acbb9ca13110f23dd6316b9aad))


### Bug Fixes

* **responses:** drop empty text blocks on Codex to Claude switch ([#141](https://github.com/pleaseai/shunt/issues/141)) ([beaeb9e](https://github.com/pleaseai/shunt/commit/beaeb9e60242ced4ac1c8a543301390a2b7b816d))

## [0.15.0](https://github.com/pleaseai/shunt/compare/v0.14.0...v0.15.0) (2026-07-14)


### Features

* **pool:** per-account thresholds + burn-rate aware account-pool load balancing ([#135](https://github.com/pleaseai/shunt/issues/135)) ([#136](https://github.com/pleaseai/shunt/issues/136)) ([3533046](https://github.com/pleaseai/shunt/commit/3533046dfe01ca9006a74a460b6ff851acd68478))
* **pool:** reconcile Claude account-pool quota via the Anthropic OAuth usage API ([#139](https://github.com/pleaseai/shunt/issues/139)) ([93f15c1](https://github.com/pleaseai/shunt/commit/93f15c1d41e49773f360320265debc6aaaf41e5c))

## [0.14.0](https://github.com/pleaseai/shunt/compare/v0.13.0...v0.14.0) (2026-07-14)


### Features

* **auth:** accept Bearer / x-api-key for inbound [server.auth] on the mapped inference path ([#130](https://github.com/pleaseai/shunt/issues/130)) ([#133](https://github.com/pleaseai/shunt/issues/133)) ([68abd45](https://github.com/pleaseai/shunt/commit/68abd4543afb8518ff54d7bb74c6a58302094536))
* **codex:** inbound Codex endpoint with account-pool passthrough ([#125](https://github.com/pleaseai/shunt/issues/125)) ([a6657d9](https://github.com/pleaseai/shunt/commit/a6657d9cebe8c93a2933039396d875d100323176))
* **retry:** bounded upstream retry/backoff for transient failures ([#48](https://github.com/pleaseai/shunt/issues/48)) ([#122](https://github.com/pleaseai/shunt/issues/122)) ([1bafd42](https://github.com/pleaseai/shunt/commit/1bafd421ed340abfcc8421225c3d9e22db20cb5c))


### Bug Fixes

* **responses:** surface backend-sent error events as gateway errors on the non-streaming JSON path ([#120](https://github.com/pleaseai/shunt/issues/120)) ([bf1be43](https://github.com/pleaseai/shunt/commit/bf1be43a3ed425989c14f0b09e366bf33fee7bc7))
* **retry:** stop retrying non-idempotent POSTs after response headers ([#128](https://github.com/pleaseai/shunt/issues/128)) ([15133eb](https://github.com/pleaseai/shunt/commit/15133eb14e35052140351ec05810fda17866bcdb))

## [0.13.0](https://github.com/pleaseai/shunt/compare/v0.12.0...v0.13.0) (2026-07-14)


### Features

* **admin:** add opt-in account provisioning web surface ([#85](https://github.com/pleaseai/shunt/issues/85)) ([583d0c5](https://github.com/pleaseai/shunt/commit/583d0c509fbc34017fc165b429e16edec40f893b))
* **codex-ws:** live-probe continuation normalization and add hit/fallback metric ([#108](https://github.com/pleaseai/shunt/issues/108)) ([76b20aa](https://github.com/pleaseai/shunt/commit/76b20aa392aa54ffc056b091a5225a928353ffd3)), closes [#45](https://github.com/pleaseai/shunt/issues/45)
* **codex:** add multi-account pooling and load balancing ([#114](https://github.com/pleaseai/shunt/issues/114)) ([3eb3f59](https://github.com/pleaseai/shunt/commit/3eb3f5998eb910f267649918ca30370647b724f5))
* **discovery:** enforce inbound auth on GET /v1/models when [server.auth] is set ([#90](https://github.com/pleaseai/shunt/issues/90)) ([#110](https://github.com/pleaseai/shunt/issues/110)) ([d9b707b](https://github.com/pleaseai/shunt/commit/d9b707be066a1d22f76d8fcc85515072883c16d4))


### Bug Fixes

* **auth:** cancellation-safe Claude OAuth refresh + off-thread store I/O ([#73](https://github.com/pleaseai/shunt/issues/73), [#101](https://github.com/pleaseai/shunt/issues/101)) ([#109](https://github.com/pleaseai/shunt/issues/109)) ([129dcfc](https://github.com/pleaseai/shunt/commit/129dcfca107aab457da47d1e2baf6c4ee4e83b8e))
* **codex-ws:** fall back to HTTP on pre-first-token websocket drop ([#46](https://github.com/pleaseai/shunt/issues/46)) ([#111](https://github.com/pleaseai/shunt/issues/111)) ([14fc926](https://github.com/pleaseai/shunt/commit/14fc926373d799d52036bce73c83864da13626dd))
* **codex:** seed message_start usage.input_tokens so codex subagents report context ([#112](https://github.com/pleaseai/shunt/issues/112)) ([bde04f9](https://github.com/pleaseai/shunt/commit/bde04f9a7e316ce87650a7bbc269392d1d952e93))
* **count_tokens:** return 501 not_supported instead of 404 when backend lacks count-tokens API ([#89](https://github.com/pleaseai/shunt/issues/89)) ([#106](https://github.com/pleaseai/shunt/issues/106)) ([892511d](https://github.com/pleaseai/shunt/commit/892511dd4c1f37a2e452e9921b0c4bbf3c722465))

## [0.12.0](https://github.com/pleaseai/shunt/compare/v0.11.0...v0.12.0) (2026-07-13)


### Features

* **codex:** map Claude ToolSearch to native Responses client tool_search ([#82](https://github.com/pleaseai/shunt/issues/82)) ([#86](https://github.com/pleaseai/shunt/issues/86)) ([ab777f2](https://github.com/pleaseai/shunt/commit/ab777f2ed266467a5b3946d71a93fda9bda5cf62))


### Bug Fixes

* **adapters:** map upstream error statuses to Anthropic error types on translated backends ([#94](https://github.com/pleaseai/shunt/issues/94)) ([057be52](https://github.com/pleaseai/shunt/commit/057be5259dbf4d06b8e52fb8a86d200e655ef17e))
* **codex-ws:** keep pooled WebSockets responsive to upstream pings ([#96](https://github.com/pleaseai/shunt/issues/96)) ([fbba7de](https://github.com/pleaseai/shunt/commit/fbba7de1f71f803a14210bd202b2c015689b5ddf))

## [0.11.0](https://github.com/pleaseai/shunt/compare/v0.10.0...v0.11.0) (2026-07-13)


### Features

* **anthropic:** label upstream 429s with rate_limit_kind in the request log ([#74](https://github.com/pleaseai/shunt/issues/74)) ([382fdb7](https://github.com/pleaseai/shunt/commit/382fdb76791d553b80492f1bf4be4f027975a707))
* **anthropic:** multi-account load balancing with quota-aware rotation ([#70](https://github.com/pleaseai/shunt/issues/70)) ([34cb9c8](https://github.com/pleaseai/shunt/commit/34cb9c860c6e10f0bc21af9d1b61e84739417f1e))
* **sentry:** opt-in performance tracing and fatal-error capture ([#75](https://github.com/pleaseai/shunt/issues/75)) ([23a175a](https://github.com/pleaseai/shunt/commit/23a175a7ca3ac9ac2a9d120b721b27e7720c0a2d))
* **xai:** enable hosted web search for Grok OAuth ([#71](https://github.com/pleaseai/shunt/issues/71)) ([908a195](https://github.com/pleaseai/shunt/commit/908a1950a66212520ab72632111fef6cb9a72a01))

## [0.10.0](https://github.com/pleaseai/shunt/compare/v0.9.0...v0.10.0) (2026-07-12)


### Features

* add Cursor provider (ConnectRPC/protobuf adapter, OAuth, tool bridging) ([#23](https://github.com/pleaseai/shunt/issues/23)) ([72c1d94](https://github.com/pleaseai/shunt/commit/72c1d9475645af694007eae33439798121e408f1))
* **codex:** emulate defer_loading for progressive tool reveal ([#43](https://github.com/pleaseai/shunt/issues/43)) ([#63](https://github.com/pleaseai/shunt/issues/63)) ([6a141d9](https://github.com/pleaseai/shunt/commit/6a141d97c815eef2a94712165c40cb36ec0f7d86))
* **otel:** opt-in OpenTelemetry (OTLP) export for traces, metrics, and logs ([#64](https://github.com/pleaseai/shunt/issues/64)) ([0bb4fdf](https://github.com/pleaseai/shunt/commit/0bb4fdfef84aaed122e3dee1244970206f6aa221))

## [0.9.0](https://github.com/pleaseai/shunt/compare/v0.8.0...v0.9.0) (2026-07-12)


### Features

* **config:** support YAML config files alongside TOML ([#41](https://github.com/pleaseai/shunt/issues/41)) ([0fc3a41](https://github.com/pleaseai/shunt/commit/0fc3a41541472f8960389dd57f0a9298428d6f2a))
* **plugins:** add per-provider shunt subagent plugins ([#55](https://github.com/pleaseai/shunt/issues/55)) ([b7aa935](https://github.com/pleaseai/shunt/commit/b7aa935366d278ddc07d437780d0b0f5f2729f80))
* **responses:** route hosted web_search off the phantom-function path ([#53](https://github.com/pleaseai/shunt/issues/53)) ([5dc7d14](https://github.com/pleaseai/shunt/commit/5dc7d14c7aa39bb0055f1ced5e6c41264b292cfd))
* **server:** serve GET /protocol gateway-protocol descriptor ([#57](https://github.com/pleaseai/shunt/issues/57)) ([e68a673](https://github.com/pleaseai/shunt/commit/e68a67304255d5b26dff0a28586a039bc7f6b9a0)), closes [#49](https://github.com/pleaseai/shunt/issues/49)
* **xai:** add grok subscription-OAuth provider via the Grok CLI proxy ([#58](https://github.com/pleaseai/shunt/issues/58)) ([90e7110](https://github.com/pleaseai/shunt/commit/90e711059fc727f56352d2fc10d81bd6e6f95db6))


### Bug Fixes

* **codex-ws:** install rustls crypto provider to prevent wss panic ([#51](https://github.com/pleaseai/shunt/issues/51)) ([2c06425](https://github.com/pleaseai/shunt/commit/2c064250faba1053fcdfed8173a3dbf1d14ddd75))

## [0.8.0](https://github.com/pleaseai/shunt/compare/v0.7.0...v0.8.0) (2026-07-11)


### Features

* **codex-ws:** previous_response_id continuation + normalization for the Codex WebSocket v2 transport ([#39](https://github.com/pleaseai/shunt/issues/39)) ([5576c37](https://github.com/pleaseai/shunt/commit/5576c377aea956f8fc01609c47f13a12a1363f62))


### Bug Fixes

* **gateway:** strip duplicate x-api-key for OAuth bearer on passthrough ([#38](https://github.com/pleaseai/shunt/issues/38)) ([8a9954e](https://github.com/pleaseai/shunt/commit/8a9954e2fa6b6b3b95ddfa44ea6b9de0804f2080))

## [0.7.0](https://github.com/pleaseai/shunt/compare/v0.6.0...v0.7.0) (2026-07-11)


### Features

* **adapters:** forward codex session/identity headers on chatgpt oauth ([#33](https://github.com/pleaseai/shunt/issues/33)) ([2ce410d](https://github.com/pleaseai/shunt/commit/2ce410d3e5f9e53c54163432b726ba23e57081f6))
* add GET /routes endpoint exposing routable model slugs ([#36](https://github.com/pleaseai/shunt/issues/36)) ([d95ee45](https://github.com/pleaseai/shunt/commit/d95ee45dc10a181eaf5bac4c00b0a52fb8ba8c82))

## [0.6.0](https://github.com/pleaseai/shunt/compare/v0.5.0...v0.6.0) (2026-07-11)


### Features

* add shunt-codex Claude Code plugin with GPT-5.6 subagents ([#21](https://github.com/pleaseai/shunt/issues/21)) ([d9adf41](https://github.com/pleaseai/shunt/commit/d9adf41a4eceabf050a5f4c6d36e020a31dfc087))

## [0.5.0](https://github.com/pleaseai/shunt/compare/v0.4.0...v0.5.0) (2026-07-11)


### Features

* **config:** hot-reload config on SIGHUP and file change ([#18](https://github.com/pleaseai/shunt/issues/18)) ([17abe55](https://github.com/pleaseai/shunt/commit/17abe550d16ec873a19526a5db578d48465e9ceb))
* strip [1m] context hint + document codex-path context accounting ([#19](https://github.com/pleaseai/shunt/issues/19)) ([01a0436](https://github.com/pleaseai/shunt/commit/01a043691e8319870132481e917d43dec371f870))

## [0.4.0](https://github.com/pleaseai/shunt/compare/v0.3.0...v0.4.0) (2026-07-10)


### Features

* **observability:** add opt-in Sentry error reporting ([#12](https://github.com/pleaseai/shunt/issues/12)) ([2b4009c](https://github.com/pleaseai/shunt/commit/2b4009cd894f8a60e834fdfa2946758562991e75))
* **observability:** add opt-in Sentry usage metrics ([#13](https://github.com/pleaseai/shunt/issues/13)) ([983319a](https://github.com/pleaseai/shunt/commit/983319addceeb883e293f16ec6ed9c21e0ad75b2))


### Bug Fixes

* **codex:** send codex client identity headers to unlock version-gated models ([#16](https://github.com/pleaseai/shunt/issues/16)) ([83e8d97](https://github.com/pleaseai/shunt/commit/83e8d97310ce5a088ac6b1c9ea1360355db92ec1))

## [0.3.0](https://github.com/pleaseai/shunt/compare/v0.2.0...v0.3.0) (2026-07-10)


### Features

* **site:** serve LLM-friendly markdown twins via Cloudflare worker ([#11](https://github.com/pleaseai/shunt/issues/11)) ([4569d02](https://github.com/pleaseai/shunt/commit/4569d027519d89c8bee25069cf5bc58e342f78cb))
* **xai:** add xAI Grok provider with SuperGrok OAuth login ([#8](https://github.com/pleaseai/shunt/issues/8)) ([a8540c1](https://github.com/pleaseai/shunt/commit/a8540c139f1811470c1b0d9b4cb849550d2cf5b3))


### Bug Fixes

* **responses:** rewrite context-overflow errors to Anthropic wording ([#9](https://github.com/pleaseai/shunt/issues/9)) ([8ef8746](https://github.com/pleaseai/shunt/commit/8ef87469acd9444e1cf57d917ff5d84cfc3b3a6b))

## [0.2.0](https://github.com/pleaseai/shunt/compare/v0.1.0...v0.2.0) (2026-07-10)


### Features

* add GET /health healthcheck and GET / landing endpoints ([#4](https://github.com/pleaseai/shunt/issues/4)) ([3618779](https://github.com/pleaseai/shunt/commit/3618779538c92bec08ae7dc85c2cb1033d39a784))
* **config:** standard config-file fallback chain and strict --config ([#5](https://github.com/pleaseai/shunt/issues/5)) ([66fa78b](https://github.com/pleaseai/shunt/commit/66fa78b8398f686d4a1ec6ea61cd6703dc20c24d))

## 0.1.0 (2026-07-09)


### Features

* add M0 pass-through Anthropic Messages gateway ([bacda61](https://github.com/pleaseai/shunt/commit/bacda61b1d8a0536f33e571669ecccc6802c9a53))
* add shunt token subcommand for Claude subscription apiKeyHelper ([7309006](https://github.com/pleaseai/shunt/commit/7309006de0825782a430aa443175d8fc4aba16a5))
* **auth:** add inbound client tokens for shared gateways (M4) ([fc6f085](https://github.com/pleaseai/shunt/commit/fc6f085d8b48a099c6fab48b4f1f095fdd319bc7))
* default count_tokens to tiktoken for responses providers ([75f0c43](https://github.com/pleaseai/shunt/commit/75f0c4367ee68ac09e651966337aa9876db90864))
* M1 — Anthropic Messages &lt;-&gt; OpenAI Responses translation ([4ec674d](https://github.com/pleaseai/shunt/commit/4ec674d960c121fa14b272d16e6bf4c2b3dfe372))
* M2 — codex/chatgpt provider via reused ChatGPT OAuth ([ac92b9d](https://github.com/pleaseai/shunt/commit/ac92b9dc0ee06e7fe63e6aa74d9619ada03f7bfb))
* M3 — GET /v1/models discovery endpoint ([c31982f](https://github.com/pleaseai/shunt/commit/c31982f976b2cd8c2b791a0da9f6abd9bb186d5c))
* map output_config.effort to responses reasoning.effort ([119c08b](https://github.com/pleaseai/shunt/commit/119c08b6cda6341766f3b9dbb26513f9208c2f59))
* opt-in tiktoken count_tokens for responses providers ([de3b6d6](https://github.com/pleaseai/shunt/commit/de3b6d64ddc5095498220b7c37d23774bba9db6a))
* **responses:** render tool_reference blocks as loaded-tool text ([ef9e70b](https://github.com/pleaseai/shunt/commit/ef9e70ba2578d972e2eae8db4fff9cefb66891a7))
* **responses:** round-trip reasoning and enrich request/response mapping ([#2](https://github.com/pleaseai/shunt/issues/2)) ([acdc0cd](https://github.com/pleaseai/shunt/commit/acdc0cde57f5dbaf75efcf0354b41da0e5c1a16e))
* short-circuit count_tokens for responses-routed models ([a28e281](https://github.com/pleaseai/shunt/commit/a28e2819c0a1a0b0534d743cbc83a9accf5bf522))
* **sse:** inject keepalive pings on idle streams (M5) ([4091fa9](https://github.com/pleaseai/shunt/commit/4091fa958ce1a1736f5121924ce5c1a0987b1af1))
* support gpt-5.6 codex slugs and their max reasoning level ([8fee803](https://github.com/pleaseai/shunt/commit/8fee80377ec00b008e3e12392a4c4474823342b7))


### Bug Fixes

* forward prompt token usage so context shows for Responses models ([f6f524b](https://github.com/pleaseai/shunt/commit/f6f524b4f10f04b52f38a88235a2e809cb623c6d))
* map system-role messages to developer for the responses backend ([c591a1c](https://github.com/pleaseai/shunt/commit/c591a1c5a38d4ce602b3f591219c704fb68cfc3d))
* **responses:** drop max_output_tokens for the ChatGPT/Codex backend ([2522ede](https://github.com/pleaseai/shunt/commit/2522ede778c01bf09e136608f846121e6d6b35e9))
* **responses:** forward upstream Retry-After through mapped errors ([65b6acc](https://github.com/pleaseai/shunt/commit/65b6acc1e373cb818e4cbed25c6ad3ae059f2a30))
* surface upstream error detail from the responses backend ([86d8c8f](https://github.com/pleaseai/shunt/commit/86d8c8f1a19865c0e74d8fe57d57ad0675460080))
