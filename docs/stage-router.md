# Content-aware stage router

Engineering note for the opt-in `[models.stage_router]`, implementing
[ADR-0004](../.please/docs/decisions/0004-content-aware-stage-router.md).
Status: **implemented.** User-facing documentation lives in the
[stage router guide](https://shunt.sh/guides/stage-router/), with the scorer's
provenance, the upstream benchmark results, and the scoring formula in
[Switchyard Integration](https://shunt.sh/guides/switchyard/); this document is
the implementation record — what the code does, why, and what a change to it must
not break. The `switchyard-libsy` dependency it scores through — how it is
pinned, how to read its API, and how to bump it — is
[`routing-algorithms.md`](routing-algorithms.md) §1.

## 1. Scope

A `[[models]]` entry carrying a `[models.stage_router]` table names two public
model ids instead of one and picks between them per request from the
conversation's recent tool-result history. Absent the table, nothing changes: the
router arm is one `Option::is_none()` on the path that already scans
`config.models`.

Out of scope, deliberately: the LLM classifier, mid-turn escalation, and
`[server.codex_endpoint]` (which uses its own routing table and never reaches
`resolve_request_chain_value`). ADR-0004 records why for each.

## 2. Module layout

| File | Role |
|---|---|
| `src/config/stage_router.rs` | `StageRouterConfig`, `StageRouterPicker`, defaults |
| `src/config.rs` | `ModelConfig.stage_router`, `validate_stage_router` |
| `src/routing/context.rs` | `RouterContext` — the `x-claude-code-*` request hints |
| `src/routing/stage.rs` | `StageContext`, `select`, `decide`, tier → target |
| `src/routing/stage/vocabulary.rs` | Claude Code tool names → categories |
| `src/routing/stage/signals.rs` | Anthropic Messages JSON → `ToolSignals` |
| `src/routing/stage/store.rs` | The decide/commit protocol and hysteresis |
| `src/routing/stage/store/entries.rs` | The pin map, its key, and the indexes that order it |
| `src/routing.rs` | `resolve_chain`'s router arm and the `Route.model` re-stamp |
| `src/proxy/failover.rs` | The single live call site |

Scoring itself is `switchyard-libsy` and is not reimplemented. The dependency is
a **git pin at an immutable `rev` on upstream `main`**, not the crates.io
release — the same form as the two `tungstenite` pins, with a `rev` and no
`branch` key, so a bump is a reviewed diff rather than a side effect of an
unrelated `cargo update` (ADR-0005 §9). 0.2.0 is the only published release and
`main` (`0.3.0`-pre) carries the algorithms ADR-0005 builds on, which the
release does not export at all. Read the checkout cargo resolved
(`~/.cargo/git/checkouts/switchyard-*/<rev>/crates/libsy/`), not the registry
source and not GitHub's default branch, when checking the API.

Three things the pin changed for this subsystem:

- `ToolSignals` gained `repeated_failure`, `tool_result_count`,
  `assistant_turn_count`, `new_count`, and `recent_new_count` (§3).
- `DecisionSource::TestsPassed` is gone. Upstream dropped the hard
  de-escalation shortcut; a passing test now only clears libsy's own capable
  hold, which shunt does not use.
- `DecisionSource::CapableHold` is new — libsy's own hysteresis, which shunt
  treats as not-evidence for the same reason `Sticky` is not evidence (§4).

`score_signal` is byte-identical to 0.2.0's, so the calibration below is
unchanged.

## 3. Signal extraction

Two passes over `messages`:

1. Walk assistant messages **backwards**, stopping once `recent_turn_window`
   assistant turns have been read. `recent` is the set of `tool_use` ids from
   those turns; a `tool_use` block missing either `id` or `name` is excluded,
   since a call this extractor cannot name is one it cannot classify.
2. Walk **forwards once over both roles**: map `tool_use.id → tool_use.name`
   from assistant blocks and record each `tool_result` against the
   `tool_use_id` it answered, reading `is_error` **off the result**. Names are
   resolved after the walk rather than during it (`name_of`), so the join does
   not depend on a call preceding its result
   (`a_result_before_its_call_still_joins`).

**A `tool_result` block does not carry the tool's name.** The `tool_use_id` join
is the single point the extractor stands on, and it has its own test
(`joins_a_result_back_to_its_call`). `is_error` living on the result rather than
the call has another (`errors_are_read_from_the_result_not_the_call`).

Returns `None` when no completed call exists, which the caller treats as
`no_signal` rather than as agreement.

`tool_result_count` and `assistant_turn_count` are filled from the walk the
extractor already does — the first is `completed.len()` (every Anthropic
`tool_result` carries a `tool_use_id`, so that is the same set libsy counts),
the second is every assistant message, not the windowed count the recency pass
keeps.

The rest are deliberately left at their zero values, with the reasoning in the
source:

- `tests_passed` would need result-*text* matching.
- `repeated_failure` is "the same hard-or-critical failure twice in the recent
  window", and libsy decides *same* by matching result text against its error
  table. Claude Code reports only a boolean `is_error` with no category, so
  there is nothing here to compare. It is a hard escalation, so a proxy that
  guessed would escalate on any two unrelated failures — and the trailing-pair
  rule already covers that case through `severity`, on evidence this extractor
  does have.
- `new_count`/`recent_new_count` count tools an operator placed in libsy's
  `tool_semantics.new` category. shunt exposes no such config yet, so the
  category is empty.

`compacted` is no longer one of them. It does not come from the transcript at
all: `stage::decide` takes it as an argument, set from the turn's
`x-claude-code-context-compacted` header or from the latch the session's pin
carries (§4). Because of that it is the one signal that can decide a turn the
extractor returned `None` for — `decide` then builds a `ToolSignals` carrying
`compacted: true` and nothing else, rather than reporting `no_signal`, since the
history right after a compaction is typically the summary alone and libsy's
compaction override has to fire on it anyway. With the flag clear that same turn
still lands on `no_signal`.
### 3.1 Vocabulary

`Observe`: `Read`, `Glob`, `Grep`, `NotebookRead`, `WebFetch`, `WebSearch`,
`BashOutput`. `Mutate`: `Edit`, `NotebookEdit` (edit-shaped), `Write`
(write-shaped). `Plan`: `TodoWrite`, `Task`, `EnterPlanMode`, `ExitPlanMode`.
Everything else, `Bash`, `Skill`, `KillShell` and `mcp__*` included, is
uncategorised, so it contributes through `pure_bash_streak` rather than through
the `new` counter — that counter is fed by `tool_semantics.new`, which shunt
does not configure yet.

Matching is `eq_ignore_ascii_case`. `Bash` stays uncategorised on purpose: only
`input.command` separates `git status` from `rm -rf` from `cargo test`, and
classifying command strings is a new fingerprinting surface with its own failure
mode. A future `tool_semantics` override is the intended door, not a default.

A failed `Task` scores severity 1.0 against 0.7 for anything else: it is a whole
delegated subagent run collapsing, not one command failing. Both grades are
gated on `recent` — a failure outside `recent_turn_window` leaves severity at
`0.0` (`severity_ignores_errors_outside_the_window`), and the "last two
completed results both failed" critical rule requires both of them to be recent
as well.

## 4. Hysteresis

`StageRouterStore` keys on `(advertised model id, SHA-256 prefix of the session
id, SHA-256 prefix of the agent id)` — both ids are hashed and never stored, at
the sixteen-byte width `accounts::stable_session_index` keeps. The third element
is all zeros for the session's own thread, so a parent and its children never
share an entry: a `Task` child sends the parent's session id with its own
`x-claude-code-agent-id`, and reads and writes a pin of its own. Before that
element existed the child's failures escalated the parent and the child's turns
advanced the parent's dwell (ADR-0005 fact 4).

Which turns count as delegated is `RouterContext::is_delegated` (§2). The
`x-claude-code-request-class` header is authoritative when it is sent —
`subagent` and `workflow` are delegated, `main`, `compaction`, and `auxiliary`
are not, and a value this build does not recognise reads as absent. Absent the
class, a non-blank `x-claude-code-agent-id` is the rule. That fallback is the
live path on every default deployment: Claude Code gates the class header (and
the agent-type and compaction ones) behind `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`
or a first-party Anthropic base URL, and gates the agent id behind neither. A
delegated turn that sent no agent id has no scope of its own and lands in the
parent's.

| Situation | Behavior |
|---|---|
| No session id (absent or blank) | Decided statelessly; the store is not touched |
| `read_only` (count_tokens) | Decided, not recorded |
| Rejected before admission | Decided, not recorded |
| Delegated turn (`Task` child, hook agent, workflow sub-agent) | Keyed and pinned apart from the parent; its own dwell, its own budget |
| Turn carrying `x-claude-code-context-compacted` | Scored with `compacted`, and the flag is latched onto the pin it commits |
| Efficient → Capable | Immediate on any scorer-made decision |
| Capable → Efficient | `dwell_turns >= min_dwell_turns` **and** confidence `>= deescalate_threshold` **and** a scorer-made decision |
| `fall_open` / `no_signal` / `ambiguous` / `capable_hold` | Cannot move a pin in either direction |
| Fingerprint mismatch or TTL expiry | That entry alone is treated as absent |

The confidence gate admits no exemption. It used to carry one, for libsy's
`tests_passed` shortcut — the single de-escalation that skipped the scorer and
so reported `confidence: None`, which a bare `confidence >= threshold` check
would have made the one reason to go cheap that could never fire. Upstream
dropped that rule, so every source `is_signal_evidence` admits now reports a
confidence and a `None` is held rather than trusted
(`a_de_escalation_without_a_confidence_score_is_held`).

`DecisionSource::CapableHold`, upstream's replacement, is not evidence either.
It means an earlier escalation is being held on the capable tier — which is what
`StageSource::Sticky` already means here — and only libsy's stateful
`StageClassifier` stamps it, which shunt does not call: shunt calls `pick_tier`
directly.

`apply` takes the estimate as a **closure of one `bool`**, not as a value. The
compaction latch is an input to scoring rather than a filter on it, and the
latched flag lives in the pin, so the pin has to be read before the turn can be
scored. The closure runs with the store lock released — `apply` copies the entry
out and drops the guard first — so scoring never happens on the mutex.

Deciding and recording are separate calls. Routing has to run before
`check_inbound_auth` — that gate reads the resolved chain to decide whether the
route injects a credential — so the tier is chosen while the caller is still
unauthenticated. `StageRouterStore::apply` therefore returns a `PendingPin`
instead of writing, and `proxy::failover` commits it only after inbound auth and
the managed-model policy have admitted the request. Without the split an
unauthenticated caller could pin a session, evict other sessions' pins by filling
the cap, and steer the next turn of any session id it guessed. Admission, not a
successful upstream response, is the boundary: the tier chosen is the tier the
turn was dispatched at, and an upstream 500 afterwards is not evidence the choice
was wrong.

The store lock is released between the two calls, so two concurrent turns of one
session can decide against the same pin and then commit in either order — and the
one that commits last is not necessarily the one that decided last. Each decision
therefore carries a `seq` from a process-wide counter, and `commit` drops a
pending pin when the entry already there has a higher one. `last_seen` cannot do
this job: two turns routinely share an `Instant`, so a timestamp comparison
cannot separate "already superseded" from "decided in the same tick" and would
drop the second turn's write.

The comparison is on `seq` alone. Scoping it to a matching `fingerprint`, so a
reconfigured table could replace an old-table pin, reads as the careful version
and is the wrong one: two requests straddling a hot reload carry different
fingerprints, so the check would not fire and the older request would overwrite
the newer table's pin — leaving an entry every later request rejects as stale.
`seq` already covers what the exemption was for, since a request decided under
the new table necessarily holds the higher one.

The fingerprint destructures `StageRouterConfig` rather than dotting into it, so a
key added to the table later fails to compile in `fingerprint` instead of quietly
letting stale pins outlive it.

Capacity is **two budgets**, not one: `MAX_TRACKED_SESSIONS = 4096` for parent
entries and `MAX_TRACKED_CHILD_PINS = 4096` for child ones, trimmed on insert —
TTL-expired entries first, then the oldest. No timer, no background task,
nothing persisted. `apply` takes `now: Instant` so dwell, expiry, and eviction
are tested against a clock the test moves.

A commit trims **only the scope it grew, and only against that scope's cap**. A
full child budget pops the oldest child and never a parent; a parent trim never
reaches a child. The reason is that eviction takes the least recently decided
entry, and a parent blocked on its children is exactly the entry that stops
refreshing: under one shared cap a wide fan-out would evict the waiting parents
and resume them on the picker default with no dwell gate consulted — the same
sub-agent-induced flip the agent-scoped key removes, arriving through eviction
instead (ADR-0005 §5). The TTL sweep is scope-blind, because expiry is a
property of the entry rather than of its budget.

The trim is O(log n) per entry removed, not a pass over the map. Three indexes
stand beside the session map — one recency order per scope and one shared expiry
order — and the trim's two halves each read one kind:

* recency — a `BTreeMap` per scope, keyed by `seq`, a total order with no ties
  that is exactly the order turns were decided, so the least recently decided entry is
  one `pop_first`. `last_seen` could not index this: two turns of one session
  routinely share an `Instant`, which is why `seq` exists at all.
* expiry — a `BTreeSet` of `(last_seen + ttl, seq)`, so the sweep stops at the
  first entry still inside its own window instead of walking the map.

They have to be separate orders, because the oldest entry can be the live one
and a newer entry the expired one. Three things produce that inversion, and only
the first needs two routers:

* two routers configured with different `session_ttl_seconds`;
* one router reloaded with a new `session_ttl_seconds` — existing entries keep
  the TTL they were recorded with (`StageSession::ttl`), so old and new entries
  age out on different clocks under a single table;
* concurrency alone. `seq` orders arrival at `apply`, not the `now` each caller
  captured before it, so two turns can take their sequence numbers in the
  opposite order from their `last_seen`.

Sweeping the front of the recency order alone would stop at the live entry,
leave the expired one behind, and then evict the live entry in its place. With
both indexes the trim drops exactly what the whole-map pass dropped: every
expired entry, then the oldest survivor if the store is still over.

An entry whose `last_seen + ttl` overflows is absent from the expiry index —
nothing bounds `session_ttl_seconds` above, and an entry that can never expire
belongs in no expiry index. The capacity trim still reaches it.

Eviction runs under the store's single process-global mutex, so the walk it
replaces serialized against every router-backed request rather than only the one
that triggered it (§9, issue #552).

### 4.1 Why the asymmetry exists

A tier flip forfeits the per-model prompt-cache prefix; it forces
`ContinuationOutcome::Fallback` because `codex_continuation::signature` hashes
`model`; and across providers it abandons the pooled socket and sticky account
slot. Scored independently every turn, a long session can cost more routed than
unrouted. Anything that makes de-escalation cheaper must re-derive this.

### 4.2 The compaction latch

The pin entry carries a `compacted: bool`. `x-claude-code-context-compacted` is
**one-shot** — Claude Code sends it on the first `main` turn after a compaction
and consumes the flag as it reads it — but libsy's compaction override is meant
to hold, because the compaction summary stays in the prefix for the turns after
it. So the turn that carries the header is scored with
`ToolSignals.compacted = true`, which reaches libsy's hard override and resolves
to `capable` with source `override`; the pin that turn commits stores
`compacted = true`; and every later turn keyed to that pin reads that flag back
into its own `ToolSignals` and resolves the same way.

`override` is signal evidence, so the escalation moves an efficient pin on the
turn it arrives rather than waiting for the transcript to agree — and it fires
even when the post-compaction transcript has no tool activity at all, which
before the latch landed on the picker default with `no_signal` (§3).

It clears with the pin and by no other means: TTL expiry
(`session_ttl_seconds`), or a config reload that changes the router table and so
the fingerprint. A `count_tokens` probe reads the latch like any other turn but
records nothing, so it cannot set one. A request with no session id has no pin
to latch onto, so its header counts for that turn alone.

## 5. Resolution and the client-facing id

`routing::resolve_chain` resolves a router's chosen target through the ordinary
ladder and then **re-stamps `Route.model` to the requested id**. `Route.model` is
what the client is told it got (`src/adapters/anthropic/mod.rs:1032`,
`src/adapters/responses/context.rs:62`) and what Claude Code restores the model
from on `--resume`; leaking the tier there is issue #172's regression. The tier
travels upstream in `upstream_model` only.

Recursion is bounded **structurally, not by a depth counter**: validation rejects
a router whose target is itself a router, so the recursive call cannot re-enter
the router arm. That validation is what makes the one-hop claim true — do not
relax it without replacing it.

Body-less entry points (`/routes`, discovery, the public `resolve_model`) pass no
context and report the picker default, which is the right answer there: the tier
the picker falls back to when no signal decides.

## 6. Validation

`validate_stage_router` rejects: a target that is itself a router (covers
self-targeting), a blank target, a threshold outside `(0.0, 1.0]` — written as
`!(v > 0.0 && v <= 1.0)` so that `NaN`, which compares false against every bound,
is rejected too — a `recent_turn_window` of `0`, and a router **id** containing
`[1m]` (the check is on the entry's own id, not its targets). Declaring both
`stage_router` and `upstream_model` on one entry is rejected separately, by
`StageRouterWithUpstreamMap`.

The router-targeting-a-router check compares the target **after**
`strip_context_window_hint`, because that is what `resolve_chain` matches on. It
has to: compared as written, `capable_target = "claude-auto[1m]"` on the entry
`claude-auto` is not equal to `claude-auto`, but it resolves to it, and a router
that resolves back to itself re-enters the router arm on every hop. That is
unbounded recursion, which aborts the process rather than failing one request —
so the one-hop property depends on these two predicates normalizing alike.
`stage_router_rejects_a_target_that_is_itself_a_router` covers the hinted forms
and `stage_router_accepts_a_context_window_hint_on_a_plain_target` is its mirror.

`min_dwell_turns` takes no lower bound. Dwell is counted from the turn that chose
the tier, so `0` and `1` both mean "no dwell floor" — degenerate but coherent,
unlike a `recent_turn_window` of `0`, which leaves the scorer nothing to read.

Four shapes are **warnings**, not errors, and all four are emitted at the
successful load boundary beside `warn_reprobe_seconds_below_floor` rather than
from `validate`. `shunt check` still reports them, because it loads before it
validates.

| Warning | Why it is not an error |
| :-- | :-- |
| `warn_stage_router_targets_unresolvable` | Resolution always falls back to `server.default_provider`, so the target is reachable |
| `warn_stage_router_identical_targets` | Both tiers flattened onto one model is degenerate, but a one-line way to test against restructuring the entry |
| `warn_stage_router_threshold_inversion` | A cost-first deployment may genuinely want de-escalation to be the easier direction |
| `warn_stage_router_shadows_exact_route` | A `[[routes]]` entry naming a router id is inert, not wrong — the router arm returns before that table is consulted |

"Load boundary" means **once per load, not once per process.** `reload::reload`
calls `Config::load`, so a config left unfixed warns again on every hot reload.
The split from `validate` is still worth having: `validate` runs strictly more
often than `load` — `Config::load` calls it, `RuntimeState::from_config` calls it
again on each reload, and `shunt check` calls it — so warning from there would
multiply each line per reload instead of emitting it once.

`warn_stage_router_identical_targets` and
`warn_stage_router_threshold_inversion` were settled together in issue #562,
over rejecting them: each rejects a configuration with a coherent operator
intent, and the realistic failure is a typo, which a warning makes visible
without taking the configuration away. `warn_stage_router_shadows_exact_route`
follows the same reading — the shadowed entry is a leftover line, not a wrong
one.

`warn_stage_router_identical_targets` compares the two targets after
`strip_context_window_hint`, the same normalization `resolve_chain` applies, so
`"m[1m]"` and `"m"` are recognized as the one destination they route to. It is
**not** case-insensitive: routing matches ids with `==`, so ids differing only in
case are genuinely different ids and warning about them would be wrong.
`warn_stage_router_threshold_inversion` reads the *effective*
`deescalate_threshold`, so raising `confidence_threshold` past the `0.75` default
warns even though only one key was written — that config inverts the design just
as much. Equal thresholds are symmetric rather than inverted and stay silent.

## 7. Observability

A decision is invisible once made. §5 re-stamps `Route.model` to the requested
id on purpose, so neither the response body nor the resolved chain says which
tier served a turn or why. Three surfaces report it.

**Response headers.** `x-gateway-routed-model` is the configured target the
chosen tier routes to, and `x-gateway-route-source` is
`StageSource::as_label`. Both are omitted for an id that carries no router —
sent empty, a client could not tell "routed to the efficient tier" from "not
router-routed". They sit beside the existing `x-gateway-upstream` /
`x-gateway-model` / `x-gateway-upstream-model` trio; `x-gateway-routed-model`
differs from `x-gateway-upstream-model` whenever the target maps its own
`upstream_model`.

**Metrics.** `shunt.stage_router.decisions` counts decisions by the router's
model id, tier, and source. That id is the one the router was matched on, so a
client-side `[1m]` hint is already stripped from it — labelling by the raw
request id would report one router as two series. `shunt.stage_router.flips` counts those that moved a
session off its pinned tier, `from` and `to` kept separate because §4.1's
asymmetry means the two directions are not expected to match. The decision
counter cannot show churn on its own: a session pinned to `capable` and one
that just arrived there are the same row. Every label is a closed set, so the
series count is bounded by the number of configured routers rather than by
session count.

**`GET /routes`.** Grows a `routers` array — `model`, `capable_target`,
`efficient_target`, `default_tier`. A router's targets need not have
`[[routes]]` entries, so `data` alone is not guaranteed to name them; a target
that does have one still appears there as itself. The field is omitted
when no router is configured, so a deployment without one serves the
byte-identical response it served before routers existed. The tunables are
deliberately absent: the endpoint answers "where can a request go", and
calibration is read from the config.

### 7.1 Both are gated on admission

The counters and the headers report only admitted requests — the same boundary
§4's pin write waits for, and for the same reason. Routing runs before
`check_inbound_auth`, which needs the resolved chain, so a request that is about
to be rejected has already been routed. Counting it would report traffic the
gateway never carried.

`count_tokens` is excluded from the counters, as it is from
`record_proxied_request`, and its response is left unstamped as well. A probe on
a session that is not yet pinned scores its own one-turn-behind history, so the
tier it resolves to is not necessarily the tier of the turn it is measuring
(§4). Reporting that as the session's routed tier would state something the
gateway has not decided.

## 8. What is not built

No per-tier latency or token histogram. `record_proxied_request` and the stream
metrics label by provider and by `Route.model`, and §5 re-stamps that to the id
the client asked for — so two turns served by different tiers of one router land
in the same series whenever both targets resolve through one provider. Where the
targets sit on different providers the `provider` label happens to separate
them, which is not a tier dimension and should not be read as one. Giving those
series a real tier label is a change to metrics this feature does not own.

## 9. Measured cost

`benches/stage_router.rs`, run with `cargo bench --features bench --bench
stage_router`. The whole path here is `pub(crate)` and both public routing entry
points pass `stage: None`, so the benchmark reaches it through
`shunt::bench_support` — a facade gated on that feature, described in its own
module docs. CodSpeed passes the feature in `.github/workflows/codspeed.yml`;
without it the target builds, runs, and registers nothing.

One local run, `--sample-count 200`, Apple silicon. CodSpeed owns regression
detection; these are orders of magnitude, not a baseline to diff against.

Two arms are newer than the table below and are measured separately in §9.2,
from their own run: `store_turn_new_child_at_capacity`, the child-budget twin
of `store_turn_new_session_at_capacity` (§4's second budget), and
`resolve_chain_routed_delegated`, the routed path driven with a `Task` child's
headers, which is read against `resolve_chain_routed` rather than on its own.
`bench_support::resolve_chain` takes a `HeaderMap` so the delegated arm can send
those hints, and `StageStore::turn` takes an `agent_id: Option<&str>` so the
store arms can address either scope.

**Read the `fastest` column.** These arms allocate, so a sample can absorb an
allocator or scheduler excursion but never finish faster than the work takes.
That drift is enough for a median to reorder two arms standing in a containment
relation, which would say a routed resolve is cheaper than the extraction it
performs. `fastest` is the low-noise estimator; medians are given alongside so
the spread stays visible.

| Benchmark | 10 turns | 50 | 200 | 800 |
|---|---|---|---|---|
| `parse_body_to_value` | 31.5 µs *(32.0)* | 155 µs *(156)* | 632 µs *(675)* | 2.63 ms *(2.97)* |
| `extract_signals` | 3.48 µs *(3.60)* | 12.6 µs *(12.8)* | 49.5 µs *(50.5)* | 196 µs *(202)* |
| `resolve_chain_routed` | 4.79 µs *(4.87)* | 14.2 µs *(14.5)* | 50.2 µs *(50.8)* | 195 µs *(198)* |
| `resolve_chain_unrouted` | 298 ns *(300)* | 297 ns *(300)* | 300 ns *(303)* | 292 ns *(298)* |

`fastest`, with the median in parentheses.

**Extraction is linear in turn count, dominates a routed resolve, and is small
against what the request already spent.** An 80× longer history costs ~56× more.
`resolve_chain_routed` sits within a few percent of `extract_signals` at every
width, which is what "dominates" means here — the two are not separable at this
resolution, and §3's second pass reads the entire `messages` array every turn, so
a session's *cumulative* extraction cost grows quadratically in its own length
even though each call is linear.

The denominator is measured, not assumed, and it is the whole expression
`src/proxy/failover.rs` evaluates: `RequestBody::parse(body.to_vec())`. Not
`serde_json::from_slice`, whose visitor skips the duplicate-key rejection, and
not the parse alone, which would drop the linear copy the buffered request pays
on the way in — both omissions shrink a number that exists only to be a
denominator. (Neither correction moved it much: the duplicate-key visitor
inspects only *top-level* keys, nested values going through stock `Value`
deserialization, and a `memcpy` is far cheaper than parsing what it copied. The
distinction matters for what is being claimed, not for the figure.) Against that,
extraction is **7.4–11.0%** across the range. So the ceiling on optimizing §3 is
roughly a tenth of a cost the gateway has already sunk by the time the router is
consulted, which is why §3 is left as it stands (issue #553).

**A request to a model with no `[models.stage_router]` table does not read
`messages` at all.** Both `resolve_chain_*` arms send the identical body naming
the identical model id; the configs differ only in whether that id's `[[models]]`
entry carries the table. `resolve_chain_unrouted` is flat at ~295 ns across an
80× range of history length, with the tightest spread of any arm here. That
flatness is the property to protect: it is what makes the feature opt-in in cost
as well as in behaviour, and any change that makes this row slope is a regression
whatever it does to the routed rows. The routed arm additionally resolves its
chosen tier's own id through `[[routes]]`, which the unrouted arm does not — that
second lookup is part of what routing costs, not a flaw in the pairing.

### 9.1 Eviction, before and after the indexes

Both arms run against a store already holding `MAX_TRACKED_SESSIONS` entries and
both build their session id outside the timed region, so what separates them is
the eviction and not the map size, the id's allocation, or its hashing.

| Benchmark | whole-map pass | indexed |
|---|---|---|
| `store_turn_existing_session` | 553 ns | 742 ns |
| `store_turn_new_session_at_capacity` | 32.8 µs | 970 ns |
| gap | 59× | 1.3× |

`fastest`; the left column is the implementation described in issue #552, the
right is §4's. Both columns were measured in one sitting, alternating between two
checkouts and reading each arm twice, because this machine's run-to-run spread on
the returning-session arm is wide enough to invent or erase a change of the size
this table reports: the *same* binary measured 602 ns and 713 ns on separate
days. The paired figures agreed to within 3 ns (baseline) and 18 ns (indexed),
which is what makes the difference below readable at all. Treat single-run
numbers from either column as unusable for this comparison.

CodSpeed ran the same two arms in simulation mode on PR #580 and reached the same
place by a different route — it counts instructions rather than timing anything,
so a scheduler excursion cannot reach it: `store_turn_existing_session` 12.6 µs →
16.6 µs (**+32%** work) and `store_turn_new_session_at_capacity` 346.6 µs → 18.3
µs. Its absolute figures are not comparable with the wall-clock table above and
are not meant to be; the agreement that matters is that two instruments with
unrelated failure modes put the regression at +32% and +34%.

A previously unseen id at capacity used to pay a `retain` walk of all 4096
entries. It now pays one `pop_first` from each index, so the eviction stops
dominating: **34× faster**, and the gap between a new session and a returning one
collapses from 59× to 1.3×. Because that work runs under the store's single
process-global mutex, the walk serialized against every other router-backed
request rather than costing only the one that triggered it — which is why the
issue framed this as blast radius and not as per-request latency.

The returning-session path pays for it: **553 ns → 742 ns**, about 34%. That is
the four index operations every write owes whether or not it evicts — retiring
the replaced entry's slot in each of the two indexes, then adding the new one to
each. It is a real regression on the common path and is reported as one. The
trade is ~190 ns on every turn against 32 µs of mutex-held work on every new
session id, and it is worth taking only because the expensive side is the side
that serializes; a store that never saturates pays the write cost and collects
nothing, which is the case for any deployment whose distinct session ids stay
under `MAX_TRACKED_SESSIONS`. That is a count of tracked sessions, not of
concurrent requests: a session is tracked from its first routed turn until it
expires or is evicted — and expired entries are swept only when an insert takes
the store over the cap — so a client that rotates its session id can accumulate
far more tracked sessions than it ever has in flight.

### 9.2 The agent-scoped key and the latch (ADR-0005 PR 1)

One local run, divan's default sampling, Apple silicon — a different run from
the tables above, so its control arms differ from theirs by run-to-run noise and
are repeated here so the comparison stays within one run. `fastest`, median in
parentheses.

| Arm | 10 turns | 50 | 200 | 800 |
|---|---|---|---|---|
| `resolve_chain_routed` | 6.41 µs *(6.53)* | 20.6 µs *(21.3)* | 76.1 µs *(79.4)* | 291 µs *(302)* |
| `resolve_chain_routed_delegated` | 6.86 µs *(6.92)* | 20.8 µs *(21.0)* | 75.1 µs *(75.6)* | 293 µs *(308)* |
| `resolve_chain_unrouted` | 290 ns *(302)* | 291 ns *(301)* | 288 ns *(300)* | 328 ns *(339)* |

| Arm | `fastest` |
|---|---|
| `store_turn_existing_session` | 796 ns |
| `store_turn_new_session_at_capacity` | 1.05 µs |
| `store_turn_new_child_at_capacity` | 1.32 µs |

Three things the run settles. `resolve_chain_unrouted` is still flat and still
where it was: the five hint headers are read inside `stage::select`, which only
a router-backed id reaches, so non-router traffic pays no header lookup. An
earlier shape of this change parsed them in `proxy::failover` for every request
and doubled this row to ~575 ns — flat, but a cost the feature is meant not to
impose on traffic that never asked for it; that is why the parse moved.

The delegated arm costs what it should: four more header lookups and a second
SHA-256 over the agent id, ~0.4 µs at 10 turns and within noise of the routed
arm from 50 turns up, where scoring dominates. A child pin at capacity evicts
in the same O(log n) as a parent one — the ~0.3 µs over the parent arm is the
extra recency index the child scope keeps — and neither eviction touches the
other scope.
