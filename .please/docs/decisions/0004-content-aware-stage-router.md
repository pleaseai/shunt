# ADR-0004: Content-aware stage router — opt-in, signal-only, session-pinned

## Status

Accepted

## Date

2026-09-12

## Context

shunt routes by one input: the `model` id on the request. `RoutingView`
(`src/routing.rs:49-51`) deserializes that field and nothing else, and
`resolve_model_chain` resolves it statically through `[[models]]` →
`[[routes]]` → `[[route_prefixes]]` → `default_provider`. Neither prompt length,
task difficulty, cost, nor latency participates.

[NVIDIA-NeMo/Switchyard][sy] fills exactly that blank — "send each LLM call to
the cheapest model likely to succeed". Its **stage router** reads a handful of
signals out of the recent tool-result history (error severity, spinning,
exploring, recent production intensity), squashes a signed score through `tanh`
into a confidence, and picks between a capable and an efficient tier. The
question this ADR settles is whether shunt adopts it, and on what terms.

[sy]: https://github.com/NVIDIA-NeMo/Switchyard

### The claim this crosses

Three surfaces currently assert the opposite of this feature:

- `docs/comparison.md` — "Routing is purely by the request's `model` id — no
  prompt-shape fingerprinting"
- `README.md` (and its `ko`/`ja`/`zh-CN` translations) — "no fragile per-agent
  system-prompt fingerprinting"
- `site/src/content/docs/index.mdx`, `getting-started/why-shunt.md`,
  `getting-started/comparison.mdx`, and their three locale copies

`docs/comparison.md` §6 classifies items like this as **"Scope-boundary (decide
before doing)"**, which is why an ADR precedes the code rather than following it.

The claim is not one thing, though, and the distinction is the part worth
keeping. What earlier Claude Code proxies did — and what shunt refuses — is
**hashing the system prompt** to guess which subagent is calling. That breaks on
every Claude Code release, because the system prompt is not a contract. What this
router reads instead is **structured protocol metadata**: `tool_use.name` and
`tool_result.is_error`, both load-bearing fields of the Anthropic Messages wire
format. Nothing here reads prompt *text*, and the router never sees the system
prompt at all.

### What a tier flip actually costs

Switchyard's own design does not have to price this, because it is not a Claude
Code gateway. shunt is, and three costs are specific to that:

1. **Prompt caching is keyed per model.** A flip forfeits the warm prefix and
   pays a full cache write (1.25× input) on the new tier.
2. **Codex continuation.** `codex_continuation::signature`
   (`src/adapters/responses/codex_continuation.rs:152-166`) hashes `model`, so a
   flip forces `ContinuationOutcome::Fallback` — correct, but it gives back
   exactly the upload-trimming shunt just earned.
3. **A cross-provider flip** additionally abandons the session's pooled
   WebSocket and its sticky account slot.

On a long Claude Code session, scoring each turn independently can therefore cost
more than not routing at all. This is the single most important fact shaping the
design below, and it is why hysteresis is not a refinement but the core.

## Decision

### 1. Opt-in only, and placed on a `[[models]]` entry

The feature is a `[models.stage_router]` table on a `[[models]]` entry. Absent
that table, behavior is unchanged — not "nearly unchanged": the router arm is a
single `Option::is_none()` on a config field that existing deployments do not
have, on the path that already scans `config.models`.

Placement on `[[models]]` is what makes the rest cheap. The router's output is
itself a **public model id**, so it re-enters the ordinary routing ladder and
inherits failover chains, account pools, adapter selection, `effort`, and
`service_tier` for free, and appears in `/v1/models` discovery with no extra
code. This is ADR-0001's "a `[[models]]` entry binds an advertised id to a
destination" with the destination chosen per request instead of per config.

Validation rejects a router whose target is itself a router, so resolution is
**structurally one hop** rather than depth-limited.

### 2. Signals only. No LLM classifier, no escalation

Only the signal-driven stage router is adopted. Two Switchyard capabilities are
explicit non-goals:

- **The LLM classifier** puts a model call and a second credential on the hot
  path. libsy already provides the way out: when the signals are inconclusive it
  returns `PickOutcome::ConsultClassifier { default_tier }`, and shunt falls open
  to that default. Declining the judge does not fight the API — it *is* the API's
  documented `fall_open`.
- **Mid-turn escalation** collides with shunt's post-2xx no-replay invariant
  (ADR-0002 / issue #218 §3). Re-driving a turn after response headers are
  committed would require either buffering the stream — which `AGENTS.md`
  forbids — or replaying a turn the client already has.

### 3. The scorer is a dependency, not a port

`switchyard-libsy = "0.2"` (crates.io, 2026-08-10). Its three scorer entry points
take `&ToolSignals` and nothing else, and every field on that struct is public,
so shunt fills it straight from the parsed request — no
`switchyard_protocol::Request`, and emphatically no `switchyard-translation`,
which would duplicate `src/model/responses_request.rs`.

The cost was measured, not estimated: **+115,376 bytes of release binary
(+0.31%)** and **+33 lock packages**, of which roughly 26 belong to the
`jsonschema` tree serving the classifier path this router never calls — and the
linker leaves that tree out entirely (zero `jsonschema` strings and zero
`num-bigint` symbols in the built binary). The porting alternative measured 137
lines. The binary result is what decided it.

The version is pinned to the `0.2` line and held out of automatic upgrades:
0.2.0 is the only published release and upstream `main` has already diverged
from it (`ToolSignals` carries four fields there that 0.2.0 does not have). If
internalizing ever becomes necessary, the move is those 137 lines and it is
confined to one module, because `ToolSignals` is filled by shunt either way.

### 4. Asymmetric hysteresis, pinned per session

Decisions are pinned per `(advertised model id, session id)` and the two
directions are deliberately unequal:

- **Efficient → Capable fires immediately.** A turn the efficient tier cannot
  serve costs more than the cache prefix an immediate flip forfeits.
- **Capable → Efficient requires all of** `dwell_turns >= min_dwell_turns`, a
  confidence at or above the stricter `deescalate_threshold` (0.75 by default,
  against 0.5 for escalation), and a decision the scorer actually made.

`fall_open` and `no_signal` are the picker's default, not evidence, and cannot
move a pin in either direction. `tests_passed` is exempt from the *confidence*
gate but not the dwell one: it is libsy's hard de-escalation shortcut, which
skips the scorer and so reports no confidence at all, and gating it on a
confidence floor would make the strongest reason to go cheap the one reason that
could never fire.

The store lives on `AppState` beside the account pool, so it survives a config
reload. Invalidation is per entry: a pin remembers a fingerprint of the router
table it was made under, so editing one router drops only its own pins.
Session ids are hashed and never stored (the same treatment
`accounts::stable_session_index` gives them), entries expire at
`session_ttl_seconds`, the store caps at 4096 sessions with eviction amortized
onto the insert path, and nothing is persisted.

### 5. The client is told the id it asked for

`Route.model` is the alias reported back to the client
(`src/adapters/anthropic/mod.rs:1032`, `src/adapters/responses/context.rs:62`),
and Claude Code records it to restore the model on `--resume`. A router-backed
route therefore re-stamps `Route.model` to the requested id; the tier travels
upstream in `upstream_model` and nowhere else. Losing this is issue #172's
regression.

A `count_tokens` probe is decided but never committed: Claude Code sends those
with a history one turn behind, so a committing probe would let stale history
drive the pin.

## Consequences

### Positive

- Long Claude Code sessions can spend the strong model where the evidence says
  it is needed and the cheap one elsewhere, without the operator choosing per
  `/model` invocation.
- The vocabulary is Claude Code's own tool names, which is where this can beat a
  provider-neutral router: `ExitPlanMode`, a failed `Task`, and the
  observe/mutate/plan split are all read directly rather than inferred.
- Existing deployments are untouched, and the claim about prompt fingerprinting
  stays true in the sense that mattered — no system-prompt text is read.

### Negative

- A dependency whose only published release has already diverged from upstream
  `main`. Mitigated by the pin, the exclusion from automatic upgrades, and the
  measured 137-line exit.
- +33 packages of supply-chain audit surface and dependabot/renovate traffic for
  a scorer that is three functions.
- The three documentation claims above are now qualified rather than absolute,
  and a reader who skims will read the qualification as a retreat.
- Tier selection is a new source of "why did this turn use that model", which
  only observability answers. The metrics and `x-gateway-route-source` header
  that answer it are a follow-on change, not this one.

### Neutral

- `[server.codex_endpoint]` is out of scope: it uses its own routing table and
  never reaches `resolve_request_chain_value`.
- `Bash` stays deliberately unclassified. `git status` observes, `rm -rf`
  mutates, `cargo test` is a settled-state signal, and only `input.command`
  separates them — classifying command strings would be a new fingerprinting
  surface with its own failure mode, and the one this ADR just declined to open.
- The router reports its picker default on body-less surfaces (`/routes`,
  discovery, the public `resolve_model`), which is the right answer there: the
  tier a fresh session starts on.

## Alternatives Considered

- **Port the scorer instead of depending on it** (137 lines). Rejected on the
  measurement: +0.31% binary and a linker that drops the unused tree. Retained as
  the exit if the dependency goes stale.
- **Adopt `switchyard-translation` too.** Rejected — it duplicates
  `src/model/responses_request.rs` head-on, and libsy does not depend on it.
- **The LLM classifier.** Rejected; see Decision 2.
- **Mid-turn escalation.** Rejected; see Decision 2.
- **Symmetric thresholds.** Rejected as the design that makes the feature cost
  more than it saves on exactly the workload it targets — see "What a tier flip
  actually costs".
- **A global router rather than a `[[models]]` entry.** Rejected: it would make
  the feature opt-out in effect, lose the "the target is a public model id"
  property that buys failover and discovery for free, and give the operator no
  way to keep one id pinned to one model.
- **Classifying `Bash` by its command string.** Rejected; see Neutral above. The
  door is left open as a future `tool_semantics` override rather than a default.
- **Rejecting the feature to preserve the documentation claim verbatim.**
  Rejected: the claim worth preserving is about system-prompt text, and this
  router reads none. Preserving the stronger reading would cost a real capability
  to protect a sentence.
