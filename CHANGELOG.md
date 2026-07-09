# Changelog

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
