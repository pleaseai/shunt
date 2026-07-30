---
name: shunt-pr289-tool-search-default-review
description: PR #289 (commit 579b281) flips tool_search default to true across 19 doc files x4 locales; clean, plus a method lesson about where live-probe evidence actually lives.
metadata:
  type: project
---

PR #289 / commit 579b281 (`feat(tool-search): default to the native tool_search protocol on OpenAI/Codex flavors`, closes #286) flips `ProviderConfig::tool_search` / `UpstreamConfig::tool_search` default from `false` to `true` (`src/config.rs` serde attr + `default_true`, all `Config::default()` struct literals, `src/config/upstreams.rs`). Gate function `Config::native_tool_search` unchanged in logic. Verified all `file:line` citations added to docs/comparison.md and getting-started/comparison.mdx (`src/config.rs:1136-1149,2792-2805`, `src/model/responses_request.rs:791-797` and `:623-643`) — all accurate.

Heading rename "Native protocol (opt-in)" -> "Native protocol" propagated correctly to all 4 locale anchors (`#native-protocol`, `#네이티브-프로토콜`, `#ネイティブプロトコル`, `#原生协议`) with no dangling old-anchor links found anywhere in the diff.

**This review's one finding was a FALSE POSITIVE — the correction is the lesson, not the accusation.** I flagged the docs claim "The native shapes were live-probe verified against the ChatGPT/Codex backend on gpt-5.6 (2026-07-13)" (docs/comparison.md, getting-started/comparison.mdx, guides/codex.mdx + 3 locale mirrors) as unsupported, reasoning that issue #286's Verification section only cross-checked `openai/codex` source, that no test or commit message corroborated a network probe, and that the date looked borrowed from the unrelated WS-continuation probe in `docs/m7-codex-websocket.md`. **That conclusion was wrong.** The probe is real and recorded in the body of merged **PR #86**, which implemented #82: turn 1 → the backend accepted the native `tool_search` tool and emitted a `tool_search_call`; turn 2 → it accepted the `tool_search_output` and the model called the loaded tool. PR #86 was created and merged 2026-07-13, so the date is right too — two genuine things happened that day.

The resolution was therefore not to delete the claim but to make it checkable: PR #289 now cites PR #86 next to the claim in all 6 surfaces.

**Where to look before calling an empirical doc claim unsupported** — the evidence usually lives in the *implementing* PR, not in the PR under review:

1. The body of the PR that shipped the feature being described (`gh pr list --search "<feature> in:title" --state merged`, then `gh pr view <n> --json body`). In this repo, live-probe results are recorded there far more often than in `docs/`, tests, or commit messages.
2. The originating issue *and* its closing PR. An issue's "Verification" section describes what was planned; that is not evidence about what was later done.
3. Only after 1 and 2 come up empty is "unsupported claim" the right finding. Even then, prefer "provenance is not discoverable from the docs — add a citation" over "the claim is invented": the weaker framing is usually the true one, and it yields a fix (cite the source) instead of deleting an accurate sentence.
