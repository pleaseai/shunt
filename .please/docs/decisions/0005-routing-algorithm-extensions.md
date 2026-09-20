# ADR-0005: Full Switchyard routing-algorithm parity on `[[models]]`

## Status

Accepted

## Date

2026-09-18

## Context

ADR-0004 gave a `[[models]]` entry one way to decide per request:
`[models.stage_router]`, signal-only, with the LLM judge and mid-turn
escalation declined. The goal is now **every routing algorithm Switchyard
ships** ([TOML schema][schema]): `passthrough`, `random`, `prefill_router`,
`llm_classifier` (capability, escalation, custom), `stage_router` with
`tool_semantics`, `handoff_notes`, `capable_hold_turns`, and a `classifier`
fallback, `auto`, `composite`, `advisor`, `noop`, and the `subagents` overlay
in both its `passthrough` and `llm_classifier` forms.

This ADR **supersedes ADR-0004 Decision 2** (no judge, no escalation) and
**amends ADR-0004 Decision 3** (the `0.2` pin). Everything else in ADR-0004
stands, and the signal-only stage router keeps its current code path and cost.

[schema]: https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/reference/toml_schema.md

### Facts that shape the design

**1. The judge family is thousands of upstream lines, not 137.** ADR-0004
ported nothing because `pick_tier` takes a plain `&ToolSignals`. The judge
algorithms do not: `LlmTaskClassifier`, `EscalationJudgeConfig`,
`AdvisorGate`, `CompositeRouter`, and the custom-classifier policy each carry
packaged prompts, verdict schemas, parsing, budgets, and transcript
truncation. Reimplementing them means re-deriving calibration shunt cannot
measure. libsy's `Algorithm` trait makes them hostable: `run_stream` yields
`Step::CallModel` for each judge call, the host serves it over its own
transport and fulfils the promise, and `Step::Done` carries the selected
model, the rewritten request, and an optional response produced while
routing. libsy performs no I/O.

**2. The published crate is not the crate that has these.** `switchyard-libsy`
0.2.0 exports none of `AdvisorGate`, `CompositeRouter`, `SubagentRouter`,
`ToolSemantics`, or `HandoffNoteConfig`; upstream `main` (`0.3.0`-pre)
exports all of them, and its `ToolSignals` carries four fields 0.2.0 lacks.
shunt is `publish = false` and already pins two git revisions for
`tungstenite`, so a git pin on libsy is available and was not before.

**3. Two algorithms serve the answer while routing.** `escalation` calls the
weak target, buffers the reply, judges the completed turn, and either serves
the buffered reply or discards it for a strong call. `advisor` buffers each
executor turn, reviews it on a trigger, and on REDO discards it. Both serve
the buffered turn afterwards in the mode the caller asked for — an SSE
replay for a streaming caller, one JSON message for a non-streaming one
(§3, §4). `AGENTS.md` says "do not buffer upstream
SSE responses unless the client requested non-streaming output." These two
cannot be built without an amendment to that rule scoped to the routes that
select them.

**4. Sub-agent turns already pollute the parent's pin.** `StageRouterStore`
keys on `(model, sha256(session_id))` (`src/routing/stage/store.rs:407`); a
`Task` child sends the parent's session id, its own agent id, and its own
history. The child's failures escalate the parent, and its turns advance the
parent's dwell. `src/protocol.rs` accepts `x-claude-code-agent-id` but nothing
reads it.

**5. `prefill_router` is a PyO3 crate.** Upstream's `crates/prefill-router`
depends on `pyo3` with `auto-initialize` and needs the Python router package
in the active environment at build and run time. Upstream gates it behind a
cargo feature; there is no other shape.

## Decision

### 1. Two lanes: pure decisions stay synchronous, driven ones use libsy's host contract

| Lane | Algorithms | Where it runs | Cost on the unrouted path |
|---|---|---|---|
| **Pure** | fixed, `stage_router` (signal-only), `random`, `auto`, `noop`, `subagents` passthrough form, `tool_semantics`, `handoff_notes`, `capable_hold_turns` | Inside `resolve_chain`, as today | One `Option` check; `resolve_chain_unrouted` stays flat |
| **Driven** | `llm_classifier` (all modes), `stage_router.classifier`, `composite`, `advisor`, `subagents` classifier form, `prefill_router` | `libsy::drive` before dispatch, in `proxy::failover` | Zero: an entry without a driven router never constructs a driver |

The pure lane keeps ADR-0004's code and benchmarks untouched. The driven lane
is one call: `drive(algorithm, request, models, serve)` where `serve` is a
shunt closure that turns each `CallModel` into an internal request (§3). The
outcome's `selected_model_ids` feed the ordinary ladder exactly as a stage
tier does, `Route.model` is re-stamped to the requested id (ADR-0004 §5), and
an outcome that carries a `response` enters the replay path (§4).

### 2. Config shape: `[models.router]` with a `type` discriminator

Four shapes were weighed. shunt's own config already discriminates this way —
`kind = "anthropic"` on a provider, `auth = { mode = "claude_oauth" }` as an
internally tagged enum with `deny_unknown_fields` (`src/config/upstreams.rs:63`)
— so the discriminator is the idiom, not an import.

| Shape | Example | Why not |
|---|---|---|
| **A. One sub-table, tagged** (chosen) | `[models.router] type = "random"` | — |
| B. One sibling table per algorithm | `[models.stage_router]`, `[models.random]`, `[models.llm_classifier]`… | Nine tables with pairwise exclusion; every new type adds eight rules |
| C. `type` flattened onto the entry | `[[models]] id = "x" type = "random" targets = […]` | Closest to upstream's `[routes.<name>]`, but serde's `flatten` forbids `deny_unknown_fields`, and the entry's own keys (`id`, `display_name`, `upstream_model`) collide with algorithm keys |
| D. Separate top-level `[[routers]]` | `[[routers]] id = "x" type = "random"` | A second table that advertises ids next to `[[models]]`, and a name one letter from `[[routes]]`, which already exists with a different meaning |

Under A, key names, defaults, and nested table names follow the upstream
schema verbatim, so its documentation applies. `type` rather than shunt's
`kind` is a deliberate exception to local convention, taken so that the
upstream `type` values and pages can be quoted unchanged; the reference page
says so once.

```toml
# Signal-only stage router — what `[models.stage_router]` becomes.
[[models]]
id = "claude-auto"
[models.router]
type = "stage_router"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
picker = "efficient_first"
confidence_threshold = 0.5
min_dwell_turns = 3                          # shunt hysteresis keys stay
[models.router.tool_semantics]
observe = ["mcp__jbcontext__code_search"]
[models.router.handoff_notes]
escalation_note = "the previous model was stalling; pick up the diagnosis"

# The same router with the judge fallback: adding this table moves the
# entry to the driven lane.
[models.router.classifier]
target = "claude-sonnet-4-6"                 # consulted, never served
base_threshold = 0.5
classify_trigger = "user_turn"

# Weighted split, session-sticky by hash.
[[models]]
id = "claude-canary"
[models.router]
type = "random"
targets = ["claude-sonnet-4-6", "gpt-5.6-terra"]
weights = [9, 1]
affinity = "session"                         # shunt addition; or "request"

# Escalation: weak first, judge the completed turn, latch strong.
[[models]]
id = "claude-escalate"
[models.router]
type = "llm_classifier"
mode = "escalation"
weak_target = "claude-sonnet-4-6"
strong_target = "claude-opus-4-8"
classifier_target = "claude-sonnet-4-6"
[models.router.escalation]
confirmations = 2

# Advisor gate: one executor, a reviewer on terminal turns.
[[models]]
id = "claude-reviewed"
[models.router]
type = "advisor"
executor_target = "claude-sonnet-4-6"
advisor_target = "claude-opus-4-8"
gate_trigger = "no_tool_call"
max_reviews = 1

# Composite: a classifier at the user turn sets the stage router's default.
[[models]]
id = "claude-composite"
[models.router]
type = "composite"
[models.router.classifier]
target = "claude-sonnet-4-6"
base_threshold = 0.5
classify_trigger = "user_turn"
[models.router.stage]
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
confidence_threshold = 0.5

# Sub-agent overlay, on any entry, with or without a router.
[models.subagents]
type = "passthrough"
target = "claude-haiku-4-5"
# or:
# [models.subagents]
# type = "llm_classifier"
# mode = "custom"
# models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-haiku-4-5"], any = ["claude-haiku-4-5", "claude-opus-4-8"] }
# default_target = "efficient"
# classify_trigger = "new_session"
# prompt = "..."
# response_schema = '''{...}'''
# policy = { type = "target_selector", selector = "/target" }
```

Three deliberate differences from upstream:

- **Targets are public model ids**, not `[targets.<name>]` references. This is
  ADR-0004 §1's property: a chosen target re-enters the ladder and inherits
  failover, pools, adapters, `effort`, and `service_tier`. Judge, classifier,
  advisor, and reviewer targets are the same kind of id and resolve the same
  way, so a judge sits on its own pool by pointing at an entry that maps one.
  There is no `llm_clients` layer; shunt's providers are that layer, and the
  `format` upstream declares per client is what shunt's adapter already knows.
- **`[models.stage_router]` stays valid** as an alias for
  `router.type = "stage_router"`, warned once per load as deprecated; both
  forms on one entry are rejected. Removing the alias is a public-key change
  and is not part of this ADR. `shunt add` and `shunt init` blueprints emit
  the new form. *Amended 2026-09-19 (PR 2): the alias was dropped before it
  shipped — no release tag contains the table's commit — so
  `[models.stage_router]` is rejected at load with an error naming the new
  form, and there is no deprecation warning.*
- **shunt's hysteresis keys stay** (`min_dwell_turns`, `deescalate_threshold`,
  `session_ttl_seconds`). Upstream's `capable_hold_turns` is accepted with a
  shunt default of `0` so existing pins behave as they do now; a
  `DecisionSource::CapableHold` decision is held, not evidence, and cannot move
  a pin.

`[models.subagents]` sits on the entry, not inside `router`, because a fixed
entry has no router table and upstream's "passthrough with subagents" is
exactly a fixed entry here. `random` carries one shunt addition,
`affinity = "session" | "request"` (default `session`): the arm is
`sha256(seed ‖ model ‖ session_id)` scaled into the weight range, so a Claude
Code session keeps one arm without a store, survives restarts, and its
`count_tokens` probes land where the turn does. `seed` is upstream's own
`random` key. Under `affinity = "session"` it is the hash salt, read from
config and `0` when unset, so the arm is a pure function of config, model
id, and session id: stable across restarts and across replicas that load the
same config; changing `seed`, `targets`, or `weights` can move the arm. A
request with no non-blank session id takes a fresh weighted draw for that
request — the blank id is never hashed as a shared key, so a 90/10 split
stays 90/10 for clients that send no session. Session affinity is
stickiness, not access control: the id is client-supplied, so a caller who
retries ids until the hash lands on the arm it wants can steer itself — but
targets are public model ids that same caller may name directly, so the
hash guards nothing the ladder does not already expose. Tier access is the
managed-model policy's job, and a weighted split under `session` affinity
is a canary aid for well-behaved clients, not an enforced ratio. Under
`affinity = "request"` it keeps upstream's meaning and reproduces the
selection sequence. `auto` is the stage-router preset with upstream's
`picker` and `confidence_threshold`. `noop` makes no upstream call and synthesizes an empty terminal assistant
message in the caller's mode: a valid SSE sequence (`message_start` through
`message_stop`) for a streaming caller, one Message JSON object otherwise.

The one-hop rule generalises: any target named by any router table that
resolves, after `strip_context_window_hint`, to a `[[models]]` entry must
resolve to one that carries no router and no `subagents` table. A target
that names no entry keeps today's behaviour — it falls through `[[routes]]`,
`[[route_prefixes]]`, and `server.default_provider` under
`warn_stage_router_targets_unresolvable` (`docs/stage-router.md` §6) — so a
deployed `[models.stage_router]` config still loads and routes; this ADR is
not a migration of those targets. Judge-only targets are not routing
destinations but are held to the same rule. `stage_router`, `random`, and
`upstream_model` on one entry: `router` and `upstream_model` are exclusive,
as `stage_router` and `upstream_model` are today.

### 3. The internal model call: `serve` for `CallModel`

**Admission comes first, from a static envelope.** Today `proxy::failover`
resolves the chain (`failover.rs:76`) before `check_inbound_auth`
(`:100`) and the managed-model policy (`:101`), because the auth gate needs
the resolved chain to know whether the route injects a credential. A driven
router cannot keep that order: it would spend judge calls and gateway-held
credentials to resolve a chain for a caller that is about to be rejected. So
a driven entry's **dependency envelope** — the requested id plus every
target its router can name: answer tiers, judge, classifier, advisor,
reviewer, `subagents` targets — is computed at validation time with no model
call. Each named target contributes every route in its effective failover
chain, resolved through the same precedence the ladder applies at request
time (`[[models]]`, then `[[routes]]`, `[[route_prefixes]]`, and
`server.default_provider`): an alias that maps several ordered providers
contributes all of them, not its primary, and a judge id that names no
entry and falls through to the default provider is a member with that
provider's credential behaviour, not an absent one. Admission runs against
the union:
the caller must authenticate if *any* member injects a credential (a passthrough answer target with a
credential-injecting judge is not a passthrough route), and the managed-model
policy is enforced on the requested public id before `drive` is entered.
The envelope governs turns, not probes: a `count_tokens` request on a
driven entry is `read_only` as it is on the stage router today — it never
enters `drive`, makes zero judge calls, resolves to the session's current
pin or, when no live pin exists, the algorithm's no-model-call decision
(the signal-only estimate on `stage_router`, which may be its default), and
is gated and dispatched against that target's first route only, the rule
`proxy::failover` applies to every probe now (`failover.rs:92`). So a
passthrough answer target with a credential-injecting judge still answers
an unauthenticated `count_tokens` probe as it does today.
`serve` then receives a non-forgeable `AdmittedContext` minted by that gate,
not a generic internal bypass; a judge call without one is a bug, not a
request. Definition of done for PR 4 includes tests that an invalid
credential and a policy-denied model each produce zero judge calls, including
the passthrough-answer + injecting-judge shape and the shape where the
injecting judge is reached only by fall-through to `server.default_provider`;
and that an unauthenticated `count_tokens` probe on that passthrough-answer +
injecting-judge entry is answered with zero judge calls, including an
unpinned probe whose history carries decisive signals; and that a
judge call carries no inbound credential slot (reserved slots plus every
`SHARED_SLOTS` name removed unconditionally, not a value match), and a judge target on a
passthrough route is rejected at validation.

`serve` builds an Anthropic Messages body from the neutral `Request` libsy
hands it, stamps the candidate model id, and dispatches it through
`proxy::failover`'s own path under the admitted context — so it uses the same
adapters, pools, and failover as client traffic, and the same metrics with
`caller = "router"`. **The caller's credential does not travel with it.**
Today `proxy::failover` computes the passthrough origin from the *first
route of the chain it is given* (`failover.rs:168`), which for an internal
call would be the judge's own route, so a caller credential presented for
the answer provider would be forwarded to whatever passthrough provider the
judge maps — and before `drive` has selected an answer there is no answer
origin to compare against. So judge, classifier, and reviewer calls start
from a header set with every inbound credential slot removed: the
reserved and configured slots `auth::slots::ShuntCredentials` strips at
the outbound boundary (`strip_reserved_slots`, `cookie` included), then
every `SHARED_SLOTS` name (`authorization`, `x-api-key`) removed
*unconditionally* — not the value-conditional `strip_consumed_slots`,
which deliberately keeps a caller's genuine upstream credential and would
keep it here too — and they run only on the credential their target's
route injects; a judge target that resolves to a
passthrough route has nothing to run on and is rejected at validation.
A gated weak or executor turn is the answer dispatch of the selected
target, so it establishes its own primary origin from that target's chain
exactly as a client turn does. **The advertised id travels with it**: the internal
dispatch carries the outer router id as the alias to render, so the adapter's
response rendering (`Route.model` at `adapters/anthropic/mod.rs:1032`,
`adapters/responses/context.rs:62`) writes the id the client asked for into
`message_start.model` *before* any frame is captured. A frame captured under
the executor's own id and replayed verbatim would hand Claude Code the wrong
model to restore on `--resume` — issue #172 again, through the replay path.
Non-streaming for judges. The escalation weak call and the advisor executor
turn are made in the caller's own `stream` mode: for a streaming caller the
rendered SSE frames are retained for replay (§4) and the neutral `Response`
for the judge is assembled from them; for a non-streaming caller the call
is non-streaming too, the single JSON message is retained, and the judge's
`Response` derives from that JSON. No SSE-to-JSON conversion is added —
the Anthropic adapter has none, and this design does not need one.

Neutral ⇄ Anthropic conversion is `switchyard-translation`'s
`anthropic_messages` format. ADR-0004 rejected that crate because it would
duplicate `src/model/responses_request.rs`; it does not here — the wire
translation Anthropic → Responses stays shunt's, and the translation crate
is used only at the routing boundary, in both directions, on the request and
the judge's reply. Two translators, two jobs.

**Bounds are per call, not per session, and cover bodies.** `judge_timeout_ms`
is an end-to-end deadline on a non-streaming judge call, headers *and* body
(a `.send()`-only timeout stops at headers, and a `200` that then stalls
would never reach `fail_open`); `judge_max_response_bytes` caps what is
collected. A gated executor or weak turn (§4) has its own three bounds:
`gated_max_bytes` on retained frames or JSON body bytes, `gated_idle_ms`
between body chunks (SSE pings do not reset it), and
`gated_max_duration_ms` wall-clock. Crossing any bound cancels the upstream
call and refunds nothing. A judge or review bound failure *after a complete
retained turn* resolves as the algorithm's `fail_open` outcome. A retained
turn itself — the escalation weak turn as much as the advisor
executor turn — is servable only after an authoritative terminal marker,
never a partial one: `message_stop` on a streaming call, a complete body
that parses as one message on a non-streaming call. A weak turn that
crosses a bound or ends before its marker is discarded before any header
is committed and escalation proceeds to the strong call; a nonterminal
advisor executor turn is discarded the same way and the request fails as
a gateway-owned upstream error in the Anthropic error shape (`502`), not a
REDO and not a failover attempt — the upstream did answer `2xx`, so
ADR-0002's post-2xx rule holds and the caller simply never saw it. A
truncated `200` is never replayed. A per-session `max_judge_calls` bounds count on top.
The six keys live on whichever table makes the internal calls,
`[models.router]` or a classifier-form `[models.subagents]`, and each
covers its own call type
(`judge_*` the judge calls, `gated_*` the executor and weak turns), with
defaults `judge_timeout_ms = 30000`, `judge_max_response_bytes = 65536`,
`gated_max_bytes = 8388608`, `gated_idle_ms = 60000`,
`gated_max_duration_ms = 600000`, and `max_judge_calls = 8`; PR 4 may
recalibrate them against its stall tests, and the reference page changes
with them. A caller disconnect cancels the in-flight internal call. Tests:
`200`-then-stall,
an endless ping stream, an oversized turn, and a disconnect mid-collection.
Judge calls count against the pool quota they run on, which is why a judge
target should map its own entry.

### 4. Buffer-and-replay, scoped to the routes that need it

`escalation` and `advisor` are opt-in **per entry**, and an entry that
selects one accepts that its gated turns are buffered upstream and served
once the verdict is in, **in the mode the caller asked for**: a
`stream: true` caller gets the retained SSE frames replayed byte-faithfully
(retained, not re-encoded); a `stream: false` caller gets the single JSON
message from a gated call made in non-streaming mode (§3), on both
adapters. The gate changes when the answer is sent, never its shape. Nothing else changes: every other
route, and every non-gated turn on these routes, streams as today.

This is the AGENTS.md amendment this ADR asks for, worded as: "do not buffer
upstream SSE responses unless the client requested non-streaming output or the
entry's router (`escalation`, `advisor`) requires the completed turn before
serving it." `x-gateway-route-source` names the verdict on a replayed turn
(`escalation_latch`, `advisor_approve`, `advisor_redo`), so a client can tell
a replayed turn from a live one. "Byte-faithful" means faithful to the frames
the client would have received live — rendered under the advertised id (§3),
so `message_start.model` is the router id, not the executor's — and the
replay test asserts that field for both an Anthropic and a Responses
executor rather than comparing bytes to the internal capture alone.

Post-2xx no-replay (ADR-0002, issue #218 §3) is not crossed: a REDO discards
a turn the client never received headers for. Headers are committed only when
the frames start replaying.

### 5. Sub-agent routing and the pin scope

A request is **delegated work** when `x-claude-code-request-class` is
`subagent` or `workflow`; when that header is absent, when
`x-claude-code-agent-id` is present and non-blank (upstream's
`claude_lineage`; Claude Code ≥ 2.1.139). The class is authoritative when
sent: `main` with an agent id is main traffic and never takes the overlay
(§11). A
`[models.subagents]` table takes `type = "passthrough"` with `target`, or
`type = "llm_classifier"` with `mode = "custom"` and the upstream `models`
groups, run on the driven lane with `classify_trigger = "new_session"` keyed
on `(session, agent)`. Parent traffic never sees the table.

Shipped first, before any new table: the stage store key gains
`sha256(agent_id)[..16]` (zero for a parent), and child pins get their own
`MAX_TRACKED_CHILD_PINS` budget evicted only against other children. Eviction
takes the oldest `last_seen` (`store.rs:363-390`), and a parent blocked on
its children is the entry that never refreshes; under one shared cap, wide
fan-out would evict waiting parents and resume them on the picker default
with no dwell gate consulted — the same sub-agent-induced flip the key change
removes, arriving through eviction.

### 6. `tool_semantics`, `handoff_notes`, `prefill_router`

`tool_semantics` accepts all four upstream categories including `new`, because
the libsy revision this ADR pins carries `new_count`; the categories apply in
`vocabulary::classify` after the built-in table, cannot reclassify a built-in
observe/mutate/plan name, and join the store fingerprint. `handoff_notes`
appends the upstream note to the forwarded system prompt on the turns a
signal drives; the cost is a prompt-cache miss on each toggle, stated in the
docs, and the attribution block is left untouched — the note is a separate
system block after it. `prefill_router` is a cargo feature `prefill-router`
that pulls upstream's PyO3 crate, off by default and absent from release
binaries; with the feature off, `type = "prefill_router"` is a load error
naming the feature.

### 7. Observability

`x-gateway-routed-model` and `x-gateway-route-source` are stamped for every
router type; sources are libsy's `DecisionSource` labels plus the closed set
`random`, `random_session`, `subagent`, `noop`, and the §4 verdicts.
`shunt.stage_router.decisions` / `.flips` stay as shipped. New:
`shunt.router.decisions{model, algorithm, target, source}` for every
algorithm including stage, and `shunt.router.judge_calls{model, algorithm,
outcome}`. `GET /routes` `routers[]` gains `algorithm`, `targets`, and a
`judges` list so a reader can see which ids are consulted but never served.

### 8. Sequencing

| PR | Scope | Definition of done |
|---|---|---|
| 0 | libsy git pin at an immutable `rev` on upstream `main`, in the form the two `tungstenite` pins use (no `branch` key); fill the four new `ToolSignals` fields; handle `DecisionSource::CapableHold` | Behaviour-preserving; the `is_signal_evidence` match compiles with the new variant |
| 1 | `RouterContext` with the §11 request hints (session, agent id, request class, agent type, compacted), agent-scoped pin key, child budget, compaction latch | Behaviour-preserving without the hints; child errors leave the parent pin untouched; child fan-out at capacity cannot evict an idle capable parent; a `context-compacted` turn escalates and the next turn of that session still reads `compacted = true` |
| 2 | `[models.router]` discriminator, `stage_router` alias, `random`, `auto`, `noop`, `tool_semantics`, `handoff_notes`, `capable_hold_turns` | Old configs load unchanged with one deprecation warning; `resolve_chain_unrouted` bench flat; sessionless requests under `random` session affinity follow the configured weights rather than one shared arm. *Amended 2026-09-19: no alias and no warning; a config writing `[models.stage_router]` fails to load with an error naming `[models.router]` (§9 Amendments)* |
| 3 | `subagents` passthrough form with `by_type` | A `subagent`/`workflow` request routes to its `by_type` target, else `target`, with no store access; `main` with an agent id, `compaction`, and `auxiliary` never take the overlay |
| 4 | Dependency envelope + admission before `drive`, internal `serve`, translation boundary, per-call bounds | Invalid credential and policy-denied model each produce zero judge calls (incl. passthrough answer + injecting judge, and a judge reached only by fall-through to `server.default_provider`); an unauthenticated `count_tokens` probe on a passthrough-answer + injecting-judge entry is answered with zero judge calls, including an unpinned probe with decisive signals; a judge call carries no inbound credential slot (reserved slots plus every `SHARED_SLOTS` name removed unconditionally, not a value match), and a judge target on a passthrough route is rejected at validation; a judge call appears as `caller = "router"` and consumes its target's pool quota; `200`-then-stall and endless-ping judges resolve as `fail_open` within the deadline |
| 5 | Driven lane: `llm_classifier` capability + custom, `stage_router.classifier`, `composite`, `subagents` classifier form | Verdict parsed from a real Anthropic tool-use reply and from an OpenAI `json_schema` reply |
| 6 | Buffer-and-replay lane: `escalation`, `advisor` | Replayed `message_start.model` equals the router id on both adapters; a `stream: false` caller receives one JSON message on a gated turn, on both adapters; REDO never commits headers; a judge or review bound failure after a complete retained turn resolves as `fail_open`; a weak turn cut before `message_stop` is never replayed and the strong call is made instead; a nonterminal advisor executor turn fails with a gateway-owned `502` before any header is committed; `AGENTS.md` amended in the same PR |
| 7 | `prefill-router` feature | Builds with the feature on a machine with the Python package; load error names the feature when off |

PR 0 through 3 need no new external dependency beyond the pin. PR 6 is gated
on the §4 amendment being accepted. Each PR updates `README.md` + locales,
`docs/routing-algorithms.md` (new engineering note), the site guides and
reference pages with `ko`/`ja`/`zh-cn` copies, and a bench arm.

### 9. Questions settled at acceptance

Four points were left open in the proposed draft and decided on 2026-09-18:

| Question | Decision |
|---|---|
| `AGENTS.md` streaming rule | Amended, route-scoped to `escalation` and `advisor`, with the §3 bounds (§4) |
| libsy dependency | Git pin on upstream `main` at a reviewed revision; bumps are deliberate rev changes |
| Discriminator key | `type`, as a documented exception to shunt's `kind`/`mode` convention (§2) |
| `prefill_router` | Cargo feature `prefill-router`, off by default, absent from release binaries (§6) |

#### Amendments

- **2026-09-19 (PR 2) — the `[models.stage_router]` alias is dropped.** §2's
  "stays valid as an alias" bullet and §8's PR 2 definition of done assumed a
  deployed spelling to keep loading. No release tag contains the commit that
  added `[models.stage_router]`, so there is none: the table is renamed
  outright to `[models.router]`, a config still writing the old form fails to
  load with an error naming the new one, and no deprecation warning exists.
- **2026-09-21 (PR 3) — the route-source set gains `subagent_type`.** §7
  names `subagent` alone for the overlay. The passthrough form reports
  `subagent_type` for a `by_type` hit and `subagent` for the `target`
  fallback — the split `random`/`random_session` already makes — so a header
  or metric reader can tell which key decided. The overlay's `algorithm`
  label is `subagents`.

### 10. Verification before code

Three external facts to capture live through `shunt run` before the
predicates are written, all recorded in `docs/notes/`: that a parent Claude
Code turn sends no `x-claude-code-agent-id` and a `Task` child does; the
literal `x-claude-code-agent-type` values the built-in agents send under
`CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` (§11 reads `fork` from the binary and
infers the rest); and that a forced tool-use reply from the current Claude
models parses as the judge verdict schema without the `json_object`
fallback.

### 11. Claude Code gateway hint headers (≥ 2.1.273)

Claude Code 2.1.273 added five request headers for LLM gateways. What
follows is read from the 2.1.274 binary, not the changelog, and is the
contract this ADR builds on; live capture (§10) confirms the values.

**The gate.** `CLAUDE_CODE_GATEWAY_HINT_HEADERS` wins when set. Otherwise the
headers are on only when the base URL is a first-party Anthropic host; behind
any other `ANTHROPIC_BASE_URL` — which is every shunt deployment — they are
**off** unless the operator sets the variable to `1`. So the docs ship the
variable alongside `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, every design
point below has an "absent" branch equal to today's behaviour, and
`/protocol` lists the five as consumed headers. Values are percent-encoded
for `%` and anything outside printable ASCII.

| Header | Values | When sent |
|---|---|---|
| `x-claude-code-request-class` | `main`, `subagent`, `workflow`, `compaction`, `auxiliary` | Every request that has a query source. `main` is the REPL main thread or the SDK; `subagent` is any `agent:*` source or a hook agent; `workflow` is a sub-agent under a workflow run; `compaction` is the compact call itself; `auxiliary` is everything else (title, summary, and other maintenance calls) |
| `x-claude-code-agent-type` | `teammate`, a built-in agent id (`fork`, `Explore`, …), `custom` | Only on `agent:*` sources. A custom agent's name is **not** exposed, only `custom` |
| `x-claude-code-compaction` | `manual`, `auto`, `reactive` | On the compaction request itself |
| `x-claude-code-context-compacted` | `manual`, `auto`, `reactive` | **Once**, on the next `main` turn after a compaction; the flag is consumed when read |
| `x-claude-code-prev-tool-durations` | `Name=ms;Name=ms…`, ≤ 32 entries, ≤ 4096 bytes | Turns that follow tool execution; the previous turn's tools and wall-clock durations |

**What each one changes here.**

- **Delegated work** (§5) is `request-class ∈ {subagent, workflow}`, falling
  back to the agent-id rule when the header is absent. `compaction` and
  `auxiliary` are *harness maintenance*: they never take the `subagents`
  overlay (upstream's `SubagentOverride` abstains on `compact` for the same
  reason), and the stage router treats them as `read_only` — decided so they
  land on the session's tier, never recorded, so a title-generation call
  cannot advance a dwell counter or evict a pin. Absent the header, they
  route as they do today.
- **`[models.subagents]` gains `by_type`**, a map from agent type to target
  with `target` as the fallback, on the pure lane:

  ```toml
  [models.subagents]
  type = "passthrough"
  target = "claude-haiku-4-5"
  by_type = { Explore = "claude-haiku-4-5", fork = "claude-sonnet-4-6", teammate = "claude-sonnet-4-6" }
  ```

  Keys are the header's literal values, matched exactly; `custom` is the only
  key a custom agent can match. Every value is held to the one-hop rule.
- **`compacted` finally has a source.** ADR-0004 left `ToolSignals.compacted`
  at `false` for want of a marker; `x-claude-code-context-compacted` is that
  marker, and libsy's hard-escalate override fires on it. But the header is
  **one-shot** where libsy's own reading is self-latching (upstream reads the
  compaction summary that stays in the prefix). So the latch lives on the
  session pin: the turn that carries the header sets `compacted` on the
  entry, later turns of that session read it back into `ToolSignals`, and it
  clears with the pin's TTL. A sessionless request latches for that turn
  only. This changes which turns escalate on a router that has been
  deployed already, so it is called out in the PR 1 release note.
- **`prev-tool-durations` is parsed and reserved.** libsy has no consumer for
  it; a stalled long-running tool is a plausible `spinning` corroborator, but
  that is a calibration change upstream owns. v1 exposes it as a histogram
  (`shunt.router.prev_tool_duration_ms{tool}`, tool label from the built-in
  vocabulary plus `other`) and nothing else.

## Consequences

### Positive

- Every Switchyard strategy is reachable from one `[[models]]` entry, with
  targets that inherit shunt's pools, failover, and discovery for free.
- Upstream prompts, schemas, and calibration are used as shipped, not
  re-derived; a future upstream algorithm is a pin bump and a `type` value.
- The signal-only stage router, the pin isolation fix, and the random split
  cost nothing on the driven lane and ship first.

### Negative

- A git-pinned dependency on an unreleased upstream `main`, replacing a
  registry pin that had one release. The pin is an immutable `rev` and
  `Cargo.toml` never names the branch, so a bump is a reviewed diff rather
  than a side effect of an unrelated update; the two-lane split keeps the
  pure lane compilable against the scorer alone.
- `switchyard-translation` enters the tree after ADR-0004 kept it out. Its
  scope is the routing boundary only, and that boundary is tested by
  round-tripping a captured Claude Code request.
- Judge calls put a model call, its latency, and its quota on the hot path of
  the routes that opt in. Budgets bound it; they do not remove it.
- Buffer-and-replay is a permanent exception to the streaming rule, scoped by
  config. A misconfigured `advisor` on a chat-only workload buffers every turn.
- `type` breaks shunt's `kind`/`mode` naming convention for one table, in
  exchange for quoting upstream's values and pages unchanged.

### Neutral

- `[server.codex_endpoint]` keeps its own routing table and is out of scope,
  as in ADR-0004; the driven lane is reachable only from the Anthropic inbound.
- `Bash` stays uncategorised by default; `tool_semantics` is the operator's
  way to say otherwise.

## Alternatives considered

- **The curated subset** (sub-agent passthrough, random, tool semantics only;
  the earlier draft of this ADR). Rejected by the goal: full parity.
- **Reimplementing the judge family in shunt** instead of hosting libsy's
  algorithms. Rejected: it re-derives prompts and calibration shunt cannot
  measure, and it decouples from every future upstream algorithm.
- **Building the neutral envelope by hand** rather than adding
  `switchyard-translation`. Rejected: the judge family reads and writes the
  full `LlmRequest` (instructions, tool calls, results, `response_format`),
  which is the translation crate's whole surface.
- **Streaming the gated turn and retracting on REDO.** Impossible: a client
  that has received frames has received them. Buffering is the only shape.
- **Sibling tables, a flattened `type`, or a top-level `[[routers]]`.**
  Rejected; the comparison is §2's table.
- **A stored affinity for `random`.** Rejected: hysteresis has no use for a
  fixed split, and the hash gives every property a store would.
- **A session-only pin scope.** Rejected: it is fact 4.

## Related

- [ADR-0004](0004-content-aware-stage-router.md) — superseded in Decision 2, amended in Decision 3; §1, §4, §5 stand.
- [ADR-0002](0002-ordered-upstreams-failover.md) — the post-2xx no-replay boundary §4 respects.
- [docs/stage-router.md](../../../docs/stage-router.md) — the signal-only path this ADR leaves in place.
- [Switchyard TOML schema](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/reference/toml_schema.md), [routing overview](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/routing_algorithms/overview.md), [advisor gate](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/routing_algorithms/advisor_gate_routing.md), [escalation](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/routing_algorithms/escalation_router_routing.md), [sub-agent routing](https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/routing_algorithms/subagent_routing.md).
