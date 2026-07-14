---
name: shunt-retry-docs-fan-out
description: retry/backoff (#48) behavior claims are restated in 4 separate shunt files that must all move together; PR #126 changed the code but updated none of them
metadata:
  type: project
---

The bounded upstream retry subsystem (issue #48, `src/retry.rs`) has its behavior described
in **four** independent locations, none of which `include!`/link to a single source of truth:

1. `src/retry.rs` — the `//!` module doc header (lines ~1-40), especially the paragraph
   (~lines 22-30) about non-idempotent POSTs and the "accepts that bounded risk deliberately"
   framing.
2. `README.md` — the "**Bounded upstream retry.**" bullet (~line 73).
3. `docs/comparison.md` — gap-analysis item **E** ("Upstream retry/backoff (done: [#48])",
   ~lines 246-251).
4. `site/src/content/docs/reference/configuration.md` — the `[providers.<name>.retry]`
   section (~line 62), which is the public-facing config reference and names the affected
   paths most explicitly ("the `passthrough`/`api_key` Anthropic path, the single-credential
   Responses path ..., and the Cursor path").

PR #126 (`fix(retry): stop retrying non-idempotent POSTs after response headers`, commit
`0452ab6`) added `RetrySafety::NonIdempotentPost` and switched the Anthropic (`src/adapters/anthropic/mod.rs`
`forward()`) and Responses (`src/adapters/responses/http.rs` `forward_http()`) call sites to it,
so those two paths now retry **only** pre-response transport errors — a response status is never
retried. Cursor deliberately keeps the old status-retry behavior (`TODO(#126, cursor)` inline
comment, pending a stable idempotency identity). The PR touched none of the 4 doc surfaces above,
so all four now make the same now-false claim: that transient statuses (429/502/503/504/529) are
retried uniformly across "the Anthropic, Responses, and Cursor single-credential paths." This is a
direct contradiction of current code for 2 of the 3 named paths — high confidence (85-92),
important/critical severity per file (the `site/reference/configuration.md` one is worst since
it's the public config reference a user would tune `max_retries` against).

Checked and ruled out as in-scope: the `ko`/`ja`/`zh-cn` mirrors of
`site/src/content/docs/*/reference/configuration.md` don't even have a `[providers.<name>.retry]`
section (122 lines vs 180 in English) — this is pre-existing translation lag from whenever #48
originally shipped, not something #126 could have drifted, so don't flag it for a retry-behavior
PR unless the localization gap itself is the object of the review.

**How to apply:** for any future shunt PR touching `src/retry.rs`'s retryable-status/safety logic,
grep for `retry|idempoten` across `README.md`, `docs/comparison.md`, and
`site/src/content/docs/reference/configuration.md` (English root only — locale mirrors lag by
design, see [[shunt_i18n_docs_structure]]) and diff each hit against the new behavior; don't stop
after checking just the module doc in `src/retry.rs` itself.
