# Changelog

## [0.41.1](https://github.com/pleaseai/shunt/compare/v0.41.0...v0.41.1) (2026-09-05)


### Bug Fixes

* **antigravity:** keep an undelivered agy handoff from failing a completed turn ([#414](https://github.com/pleaseai/shunt/issues/414)) ([07ac6d4](https://github.com/pleaseai/shunt/commit/07ac6d4605fbe0953fc7a89fb23b74b85c728abc))

## [0.41.0](https://github.com/pleaseai/shunt/compare/v0.40.2...v0.41.0) (2026-09-05)


### Features

* **config:** add Zhipu and MiniMax China Anthropic presets ([#455](https://github.com/pleaseai/shunt/issues/455)) ([c0181e8](https://github.com/pleaseai/shunt/commit/c0181e8764844f00bd787a7a5fc112ba61336f02))


### Bug Fixes

* **antigravity:** re-discover the effort matrix when a turn names an unknown model ([#370](https://github.com/pleaseai/shunt/issues/370)) ([42787f6](https://github.com/pleaseai/shunt/commit/42787f642426197121bc79845b8c34865c6baffe))
* **cursor:** send composer fast mode as model metadata, not a model id ([#411](https://github.com/pleaseai/shunt/issues/411)) ([8bcacff](https://github.com/pleaseai/shunt/commit/8bcacff7e47815aa677884e1cd7ded8fea3a4be5))
* **cursor:** surface built-in tool calls instead of dropping the turn ([#410](https://github.com/pleaseai/shunt/issues/410)) ([4991462](https://github.com/pleaseai/shunt/commit/49914626460e5c52bf0709845d366aee01290848))

## [0.40.2](https://github.com/pleaseai/shunt/compare/v0.40.1...v0.40.2) (2026-09-05)


### Bug Fixes

* **antigravity:** reject caller-supplied tools instead of silently ignoring them ([#405](https://github.com/pleaseai/shunt/issues/405)) ([140098f](https://github.com/pleaseai/shunt/commit/140098fc78a9e195862269090bcde1bdbbaf5bf1))
* **codex:** bump client identity to 0.153.3 for gpt-6-astra ([#459](https://github.com/pleaseai/shunt/issues/459)) ([72134ff](https://github.com/pleaseai/shunt/commit/72134fffb918dfbdcd1d8df93f10f0b5e5165cf3))
* **codex:** classify in-stream rate_limit_exceeded as 429 and forward misalignment steer ([#463](https://github.com/pleaseai/shunt/issues/463)) ([d8105de](https://github.com/pleaseai/shunt/commit/d8105de9bfa3ad1480bc2bb6c1b19dc1ffb42353))
* **gemini:** derive the items schema Gemini requires from a tuple-style array ([#454](https://github.com/pleaseai/shunt/issues/454)) ([3421716](https://github.com/pleaseai/shunt/commit/34217167ecb537395106990d9128a24853195473))

## [0.40.1](https://github.com/pleaseai/shunt/compare/v0.40.0...v0.40.1) (2026-09-03)


### Bug Fixes

* **antigravity:** resolve model ids from the live catalog and redirect a production-pinned base_url ([#451](https://github.com/pleaseai/shunt/issues/451)) ([6d1fab3](https://github.com/pleaseai/shunt/commit/6d1fab312358c46096994dc5f3c4501134583c0b))

## [0.40.0](https://github.com/pleaseai/shunt/compare/v0.39.2...v0.40.0) (2026-09-02)


### Features

* **codex:** poll the wham usage endpoint for codex account quota ([#430](https://github.com/pleaseai/shunt/issues/430)) ([bf04cf8](https://github.com/pleaseai/shunt/commit/bf04cf8726f7f5efa49cb6ae04a8be23cb8428b0))


### Bug Fixes

* **antigravity:** reach the daily backend with the agent envelope and effort-suffixed model ids ([#449](https://github.com/pleaseai/shunt/issues/449)) ([a4be60a](https://github.com/pleaseai/shunt/commit/a4be60a863053fff2fb9b6ccc2818494059be57a))
* **gemini:** merge adjacent user turns to keep tool pairing across mid-conversation system messages ([#446](https://github.com/pleaseai/shunt/issues/446)) ([17bb0dc](https://github.com/pleaseai/shunt/commit/17bb0dcd46f19356aa52f5e5b85c6da0b0971644))

## [0.39.2](https://github.com/pleaseai/shunt/compare/v0.39.1...v0.39.2) (2026-09-01)


### Bug Fixes

* **admin:** keep a needs-relogin verdict for a never-selected account ([#444](https://github.com/pleaseai/shunt/issues/444)) ([7fd812c](https://github.com/pleaseai/shunt/commit/7fd812cfb471f0ca2d438a34f234e449d5410127))

## [0.39.1](https://github.com/pleaseai/shunt/compare/v0.39.0...v0.39.1) (2026-08-31)


### Bug Fixes

* **admin:** report Claude account status by credential kind, not raw expiry ([#437](https://github.com/pleaseai/shunt/issues/437)) ([06fd8e0](https://github.com/pleaseai/shunt/commit/06fd8e0c12759e5c581aae4c245faed4d6a810e3))

## [0.39.0](https://github.com/pleaseai/shunt/compare/v0.38.0...v0.39.0) (2026-08-29)


### Features

* **pool:** opportunistically re-probe stale near-quota accounts ([#429](https://github.com/pleaseai/shunt/issues/429)) ([7dedde3](https://github.com/pleaseai/shunt/commit/7dedde3bbe16773005b9ecee6b49dc5753e805aa))


### Bug Fixes

* **admin:** key pool plans by account identity and single-flight the file read ([#423](https://github.com/pleaseai/shunt/issues/423)) ([412a0e1](https://github.com/pleaseai/shunt/commit/412a0e1c21685df9565a061a611d052248c3a500)), closes [#420](https://github.com/pleaseai/shunt/issues/420) [#421](https://github.com/pleaseai/shunt/issues/421) [#422](https://github.com/pleaseai/shunt/issues/422)
* **anthropic:** strip deferred tools on non-Anthropic Messages models ([#415](https://github.com/pleaseai/shunt/issues/415)) ([fe35207](https://github.com/pleaseai/shunt/commit/fe3520776b135bc177b948f92725e07a03bf52c9))
* **pool:** bound the lifetime of reset-less quota marks ([#428](https://github.com/pleaseai/shunt/issues/428)) ([ad88866](https://github.com/pleaseai/shunt/commit/ad88866b2477c435ffd4cdaffdcb23cf5c05ccd4))

## [0.38.0](https://github.com/pleaseai/shunt/compare/v0.37.0...v0.38.0) (2026-08-25)


### ⚠ BREAKING CHANGES

* **auth:** a shared slot holding a shunt credential in any position is now removed entirely, so a genuine upstream credential sharing that slot is dropped with it. Only affects callers who send `authorization` or `x-api-key` more than once in a single request. The surgical alternative — rebuilding the slot from its surviving values — adds a second place deciding which values are shunt's, which is the drift `auth::slots` exists to remove.
* **antigravity:** `kind = "antigravity"` is now the native HTTP upstream, and the local `agy` subprocess transport moves to `kind = "antigravity_cli"` (built-in provider `antigravity-cli`), which is deprecated. A config still carrying the old meaning is refused by name rather than retargeted, and a routed `antigravity` provider with no credential refuses to start — switching transport, credentials, and egress underneath a green startup would be worse than failing.

### Features

* **admin:** add read/write admin keys and move the spend surface to [server.spend] ([#389](https://github.com/pleaseai/shunt/issues/389)) ([910b90a](https://github.com/pleaseai/shunt/commit/910b90a67d2335700c45ecc23fa9a252edb29e64))
* **admin:** expose account plans in pool ([#408](https://github.com/pleaseai/shunt/issues/408)) ([7e62d00](https://github.com/pleaseai/shunt/commit/7e62d00b69c2f069fcd976b78969bcd16af64298))
* **antigravity:** reach Antigravity over HTTP instead of the local agy CLI ([#372](https://github.com/pleaseai/shunt/issues/372)) ([2708d5e](https://github.com/pleaseai/shunt/commit/2708d5eeda3805ccdc6cb6aca2237a9cb5bdaf5f))
* **auth:** mint a dedicated shunt claim so the JWT shape check widens ([#367](https://github.com/pleaseai/shunt/issues/367)) ([8fbf77f](https://github.com/pleaseai/shunt/commit/8fbf77f982025ea5c5066c37ae35368daf53d297)), closes [#365](https://github.com/pleaseai/shunt/issues/365)
* **cli:** add shunt gateway login, token helper, and Claude Code launcher ([#393](https://github.com/pleaseai/shunt/issues/393)) ([0c69989](https://github.com/pleaseai/shunt/commit/0c6998962f1217d7ee8adefad3c69720a511f4e1))
* **codex:** sync the Codex client surface to openai/codex 0.148.0 ([#403](https://github.com/pleaseai/shunt/issues/403)) ([3fe027f](https://github.com/pleaseai/shunt/commit/3fe027fabeba9ac67f70fa685396d1567f256728))
* **config:** add [server.gateway.session] JWT config block ([#359](https://github.com/pleaseai/shunt/issues/359)) ([c59f562](https://github.com/pleaseai/shunt/commit/c59f562aefd1857f1d3c6e161867f43930cc0db8))
* **config:** resolve ${VAR}/${file:} references and redact Secret fields ([#348](https://github.com/pleaseai/shunt/issues/348)) ([4c477db](https://github.com/pleaseai/shunt/commit/4c477db75c659a8ab1c603d17a33feb4c56df368))
* **gateway:** add spend-limit admin API (stage 1) ([#333](https://github.com/pleaseai/shunt/issues/333)) ([9c425e7](https://github.com/pleaseai/shunt/commit/9c425e703caa629fb89a03488c77337eb13bbc2e))
* **kimi:** add Kimi Code OAuth as a first-class subscription upstream ([#376](https://github.com/pleaseai/shunt/issues/376)) ([3e7ae16](https://github.com/pleaseai/shunt/commit/3e7ae16ab8a6670b50979b1701547e080bb2de0f))


### Bug Fixes

* **antigravity:** honor a configured base_url during credential-path project discovery ([#390](https://github.com/pleaseai/shunt/issues/390)) ([437e0bc](https://github.com/pleaseai/shunt/commit/437e0bcb1cd5bcaa2b93701660a425e5e483f461))
* **auth:** never forward a gateway JWT to an upstream in either credential slot ([#355](https://github.com/pleaseai/shunt/issues/355)) ([c065cc8](https://github.com/pleaseai/shunt/commit/c065cc89742552680926ff8ff59b7b3cd9ffd131))
* **auth:** route every credential-slot forward site through one shared strip ([#391](https://github.com/pleaseai/shunt/issues/391)) ([9fe69f8](https://github.com/pleaseai/shunt/commit/9fe69f8880b1331635f1304cd75a968a3d3e1de5))
* **auth:** strip a gateway JWT by shape, not just by whether it authenticates ([#364](https://github.com/pleaseai/shunt/issues/364)) ([c00b03b](https://github.com/pleaseai/shunt/commit/c00b03bebffcbadcbb10d4d8a8a2adeb7dc3b018))
* **auth:** strip a static `[server.auth]` token on passthrough by value ([#361](https://github.com/pleaseai/shunt/issues/361)) ([178af03](https://github.com/pleaseai/shunt/commit/178af039ad6edc1996b9134be85342b38b1ca020))
* **cli:** make `shunt check` run the routed-Antigravity credential guard ([#406](https://github.com/pleaseai/shunt/issues/406)) ([ecf5664](https://github.com/pleaseai/shunt/commit/ecf5664d6f4ce8f9add32e6293bdb7283dfd18ad)), closes [#382](https://github.com/pleaseai/shunt/issues/382)
* **codex-endpoint:** strip inbound x-api-key on the Codex passthrough ([#362](https://github.com/pleaseai/shunt/issues/362)) ([73622f1](https://github.com/pleaseai/shunt/commit/73622f17d85cf586cbe489baf06abfdaa65cb91a))
* **codex:** accept lowercase auth_mode in import_auth ([#409](https://github.com/pleaseai/shunt/issues/409)) ([e59d844](https://github.com/pleaseai/shunt/commit/e59d8449a1875372f23dba090a111b81a5d4dc9a))
* **lint:** box oversized Err variants for clippy::result_large_err ([#418](https://github.com/pleaseai/shunt/issues/418)) ([960802f](https://github.com/pleaseai/shunt/commit/960802f0fb51badb3ca402ed506a053b979369c0))

## [0.37.0](https://github.com/pleaseai/shunt/compare/v0.36.0...v0.37.0) (2026-08-13)


### Features

* **status:** add opt-in upstream Statuspage polling ([#341](https://github.com/pleaseai/shunt/issues/341)) ([a233bef](https://github.com/pleaseai/shunt/commit/a233bef78ee4a24d84b1a88353f34455ab4d825b))
* **xai:** add grok-4.6 and refresh the Grok model surface ([#343](https://github.com/pleaseai/shunt/issues/343)) ([29c456f](https://github.com/pleaseai/shunt/commit/29c456f53f514e1c48d19e151128f55298db7578))

## [0.36.0](https://github.com/pleaseai/shunt/compare/v0.35.0...v0.36.0) (2026-08-11)


### Features

* **antigravity:** run agy in agentic mode with streaming, sandboxing, and discovered effort matrix ([#325](https://github.com/pleaseai/shunt/issues/325)) ([109c38d](https://github.com/pleaseai/shunt/commit/109c38d37ed39629bd012008a10ff8dd7d1b1a6f))

## [0.35.0](https://github.com/pleaseai/shunt/compare/v0.34.1...v0.35.0) (2026-08-10)


### ⚠ BREAKING CHANGES

* **server:** The inbound request body cap now defaults to 32 MiB instead of the hardcoded 64 MiB limit. Requests between those sizes return 413 until [server.limits] max_request_bytes is raised.

### Features

* **server:** add HTTP tuning config surface ([#334](https://github.com/pleaseai/shunt/issues/334)) ([82f55d6](https://github.com/pleaseai/shunt/commit/82f55d60b47845a12e9e876423293d1f598df103))

## [0.34.1](https://github.com/pleaseai/shunt/compare/v0.34.0...v0.34.1) (2026-08-08)


### Bug Fixes

* **anthropic:** restore identity on auto-mode classifier requests ([#331](https://github.com/pleaseai/shunt/issues/331)) ([b3964ed](https://github.com/pleaseai/shunt/commit/b3964eda321fc5c42c4bb52f904c57a5bec8b66f))

## [0.34.0](https://github.com/pleaseai/shunt/compare/v0.33.2...v0.34.0) (2026-08-08)


### Features

* **gateway:** inbound OTLP telemetry ingest and verbatim relay ([#189](https://github.com/pleaseai/shunt/issues/189)) ([#326](https://github.com/pleaseai/shunt/issues/326)) ([7b4fe36](https://github.com/pleaseai/shunt/commit/7b4fe3674dfc4677d0e9cdfb53251883dce3927e))


### Bug Fixes

* **pool:** scope Fable quota exhaustion to Fable traffic ([#329](https://github.com/pleaseai/shunt/issues/329)) ([cfacde9](https://github.com/pleaseai/shunt/commit/cfacde9bb864504cf73abac0d4e9f29856809eed))

## [0.33.2](https://github.com/pleaseai/shunt/compare/v0.33.1...v0.33.2) (2026-08-07)


### Bug Fixes

* **observability:** reconcile overflow-bucket capacity with debt resurfacing ([#314](https://github.com/pleaseai/shunt/issues/314)) ([9676539](https://github.com/pleaseai/shunt/commit/9676539e985ebf8c371332af1f7cc88e5d4c1ff9))

## [0.33.1](https://github.com/pleaseai/shunt/compare/v0.33.0...v0.33.1) (2026-08-06)


### Bug Fixes

* **observability:** distinguish SSE cut kinds and attach stream context ([#310](https://github.com/pleaseai/shunt/issues/310)) ([#311](https://github.com/pleaseai/shunt/issues/311)) ([a1f16dc](https://github.com/pleaseai/shunt/commit/a1f16dcc4a2da6e1d2a2ac300d2bbf0917863cb7))

## [0.33.0](https://github.com/pleaseai/shunt/compare/v0.32.0...v0.33.0) (2026-07-31)


### Features

* **codex:** add service_tier config for Codex fast mode ([#301](https://github.com/pleaseai/shunt/issues/301)) ([631c64f](https://github.com/pleaseai/shunt/commit/631c64f798af2686f48c10c22bc20328dd5140c7))
* **observability:** surface mid-stream SSE failures in spans and Sentry ([#287](https://github.com/pleaseai/shunt/issues/287)) ([#295](https://github.com/pleaseai/shunt/issues/295)) ([ce97640](https://github.com/pleaseai/shunt/commit/ce9764026b02bf01020c88aa991b4a81bc2a3def))


### Performance Improvements

* **codex-ws:** enable permessage-deflate on the Codex WebSocket transport ([#297](https://github.com/pleaseai/shunt/issues/297)) ([1ea3d5d](https://github.com/pleaseai/shunt/commit/1ea3d5d4256d87fc0c1b0181b9a471a85ecb427b))

## [0.32.0](https://github.com/pleaseai/shunt/compare/v0.31.0...v0.32.0) (2026-07-30)


### Features

* add Homebrew services integration (brew services) ([#288](https://github.com/pleaseai/shunt/issues/288)) ([b08fd35](https://github.com/pleaseai/shunt/commit/b08fd35ba7930278cc02b9a4c3781b0b4017ea01))
* **tool-search:** default to the native tool_search protocol on known OpenAI/Codex hosts ([#289](https://github.com/pleaseai/shunt/issues/289)) ([318aa41](https://github.com/pleaseai/shunt/commit/318aa413eb2326a4797790887f86423989abc8fc)), closes [#286](https://github.com/pleaseai/shunt/issues/286)


### Performance Improvements

* **codex:** adopt zstd request compression on the Responses path ([#291](https://github.com/pleaseai/shunt/issues/291)) ([abda381](https://github.com/pleaseai/shunt/commit/abda381692f2d17b2f7b88b7fd6bf8c976615652))

## [0.31.0](https://github.com/pleaseai/shunt/compare/v0.30.0...v0.31.0) (2026-07-30)


### Features

* **admin:** group the accounts and usage table by provider ([#242](https://github.com/pleaseai/shunt/issues/242)) ([1ff8e5a](https://github.com/pleaseai/shunt/commit/1ff8e5acd372044657547777f3c3812c27704d7c))
* **observability:** tag proxy spans with model and upstream status ([#281](https://github.com/pleaseai/shunt/issues/281)) ([#284](https://github.com/pleaseai/shunt/issues/284)) ([58bff3b](https://github.com/pleaseai/shunt/commit/58bff3b23870b5c86e5c5383ef90949e01032bdc))

## [0.30.0](https://github.com/pleaseai/shunt/compare/v0.29.0...v0.30.0) (2026-07-30)


### Features

* **metrics:** add shunt.codex_ws_overflow counter for the Codex WS overflow path ([#283](https://github.com/pleaseai/shunt/issues/283)) ([b5b7ec4](https://github.com/pleaseai/shunt/commit/b5b7ec45147ba17c3b5d9c61af4336c8cda3adad))


### Bug Fixes

* **codex:** seed input-token estimate on the account-pool path ([#279](https://github.com/pleaseai/shunt/issues/279)) ([c9fad4c](https://github.com/pleaseai/shunt/commit/c9fad4c2cb0c64a74cbe5bb86aceb5f04d0a9d20))

## [0.29.0](https://github.com/pleaseai/shunt/compare/v0.28.0...v0.29.0) (2026-07-29)


### ⚠ BREAKING CHANGES

* **cursor:** move request protobuf framing off the async runtime ([#272](https://github.com/pleaseai/shunt/issues/272))

### Performance Improvements

* **cursor:** move request protobuf framing off the async runtime ([#272](https://github.com/pleaseai/shunt/issues/272)) ([1a1d8ee](https://github.com/pleaseai/shunt/commit/1a1d8ee8311b2de232ef647375aee502560a1c81))

## [0.28.0](https://github.com/pleaseai/shunt/compare/v0.27.0...v0.28.0) (2026-07-29)


### ⚠ BREAKING CHANGES

* **codex-ws:** share stored continuation via Arc instead of deep-cloning the transcript per turn ([#270](https://github.com/pleaseai/shunt/issues/270))

### Bug Fixes

* **server:** bound inbound request concurrency ([#271](https://github.com/pleaseai/shunt/issues/271)) ([916e238](https://github.com/pleaseai/shunt/commit/916e2382c4f127293afcf31a074d74249b4eca6c))


### Performance Improvements

* **codex-ws:** borrow the translated body when building the ws frame ([#273](https://github.com/pleaseai/shunt/issues/273)) ([fdc22cd](https://github.com/pleaseai/shunt/commit/fdc22cdf6fab7979b3048728463e671394e49331))
* **codex-ws:** share stored continuation via Arc instead of deep-cloning the transcript per turn ([#270](https://github.com/pleaseai/shunt/issues/270)) ([b84a3bd](https://github.com/pleaseai/shunt/commit/b84a3bde258e9501c2534e7f4d02a7bc0311702f))

## [0.27.0](https://github.com/pleaseai/shunt/compare/v0.26.1...v0.27.0) (2026-07-29)


### ⚠ BREAKING CHANGES

* **proxy:** `adapters::Adapter` is now `pub(crate)`. `Adapter::forward` takes the crate-private `RequestBody` instead of `Vec<u8>`, which forces the trait itself to be crate-private. External crates can no longer name or implement it. Practical impact is nil — adapter dispatch is static and in-crate, with no `dyn Adapter` and no public registration hook, so an external implementation could never have been invoked.

### Performance Improvements

* **proxy:** parse the inbound request body once across the hot path ([#261](https://github.com/pleaseai/shunt/issues/261)) ([ca37b70](https://github.com/pleaseai/shunt/commit/ca37b704aa8cfb511880853312f65b8161ee2c75))

## [0.26.1](https://github.com/pleaseai/shunt/compare/v0.26.0...v0.26.1) (2026-07-29)


### Performance Improvements

* **cursor:** decompress gzip response frames off the async runtime ([#256](https://github.com/pleaseai/shunt/issues/256)) ([c36c619](https://github.com/pleaseai/shunt/commit/c36c619442e1cf56ba7ebc71e8161dcea61c7ac7))
* **responses:** serialize translated request once ([#257](https://github.com/pleaseai/shunt/issues/257)) ([2c3bd73](https://github.com/pleaseai/shunt/commit/2c3bd73bf239a8f0e693624f5cdef0f1c5a73eed))

## [0.26.0](https://github.com/pleaseai/shunt/compare/v0.25.0...v0.26.0) (2026-07-28)


### Features

* **discovery:** fetch credential-scoped Anthropic models ([#244](https://github.com/pleaseai/shunt/issues/244)) ([014252d](https://github.com/pleaseai/shunt/commit/014252df7d1416f77bb88325d0e598a1e01bc42f))


### Bug Fixes

* **proxy:** move count_tokens tiktoken encoding off the async runtime ([#247](https://github.com/pleaseai/shunt/issues/247)) ([f4cd619](https://github.com/pleaseai/shunt/commit/f4cd6191421e02f726e8668ff940bea4366f87ff)), closes [#246](https://github.com/pleaseai/shunt/issues/246)


### Performance Improvements

* **ws:** open a dedicated socket instead of queueing concurrent turns ([#250](https://github.com/pleaseai/shunt/issues/250)) ([a469eab](https://github.com/pleaseai/shunt/commit/a469eab7a5fa9f2123065e1e53d3c85f2a0589df))

## [0.25.0](https://github.com/pleaseai/shunt/compare/v0.24.0...v0.25.0) (2026-07-28)


### Features

* **admin:** auto-link provider usage ([#236](https://github.com/pleaseai/shunt/issues/236)) ([e7f01f1](https://github.com/pleaseai/shunt/commit/e7f01f1cd7c5546839e9a4cd5a5caefec8195b23))
* **gemini:** add Code Assist and Antigravity providers ([#234](https://github.com/pleaseai/shunt/issues/234)) ([93fe325](https://github.com/pleaseai/shunt/commit/93fe325fab57acfdca2d75904fd741225a8811da))


### Bug Fixes

* **gemini:** preserve thought signatures across tool turns ([#240](https://github.com/pleaseai/shunt/issues/240)) ([466d67c](https://github.com/pleaseai/shunt/commit/466d67ccb50f993aa2474285899ccce78ae870b6))

## [0.24.0](https://github.com/pleaseai/shunt/compare/v0.23.0...v0.24.0) (2026-07-22)


### Features

* **admin:** show time-until-reset per quota window in pool dashboard ([#206](https://github.com/pleaseai/shunt/issues/206)) ([3272daa](https://github.com/pleaseai/shunt/commit/3272daa4a1ad143e4b1ee5ec75f15090e0241728))
* **cli:** add `shunt add` blueprint discovery command ([#225](https://github.com/pleaseai/shunt/issues/225)) ([0302c3a](https://github.com/pleaseai/shunt/commit/0302c3a90821020d5f8a173dbbbebead22080419))
* **cli:** add `shunt init` starter-config command ([#228](https://github.com/pleaseai/shunt/issues/228)) ([1d2ce53](https://github.com/pleaseai/shunt/commit/1d2ce534423dda7761aa3145724faf2b88150425))
* **config:** ordered [[upstreams]] cross-provider failover ([#218](https://github.com/pleaseai/shunt/issues/218)) ([#224](https://github.com/pleaseai/shunt/issues/224)) ([2a90a1a](https://github.com/pleaseai/shunt/commit/2a90a1a2cb05716d6eb5a1083ccd8c14b34aee48))
* **config:** unify model discovery and routing with a per-provider upstream_model map on [[models]] ([#217](https://github.com/pleaseai/shunt/issues/217)) ([21ded1d](https://github.com/pleaseai/shunt/commit/21ded1d5cca576c6101b2787dfdd7fdc6b82aaa5))
* **discovery:** auto-include builtin Claude model catalog ([#208](https://github.com/pleaseai/shunt/issues/208)) ([4904fa2](https://github.com/pleaseai/shunt/commit/4904fa2427b219fb6ac74e8ede2da3d1dd5af323))
* **discovery:** match reference gateway list shape on /v1/models ([#215](https://github.com/pleaseai/shunt/issues/215)) ([3664357](https://github.com/pleaseai/shunt/commit/366435775a156a09c0c4d275267a99c83d0985f3))
* **site:** migrate docs site from Starlight to Cloudflare Nimbus ([#231](https://github.com/pleaseai/shunt/issues/231)) ([8f345b8](https://github.com/pleaseai/shunt/commit/8f345b8f73176748f985b9a10043d4ce14738418))
* **usage:** synthesize native /usage bars via GET /api/oauth/usage ([#227](https://github.com/pleaseai/shunt/issues/227)) ([267a8f7](https://github.com/pleaseai/shunt/commit/267a8f79359e2b7c38644726962bf7647f5924a7))

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
