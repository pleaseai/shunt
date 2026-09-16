# Content-aware stage router

Engineering note for the opt-in `[models.stage_router]`, implementing
[ADR-0004](../.please/docs/decisions/0004-content-aware-stage-router.md).
Status: **implemented.** User-facing documentation lives in the
[stage router guide](https://shunt.dev/guides/stage-router/); this document is
the implementation record — what the code does, why, and what a change to it must
not break.

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
| `src/routing/stage.rs` | `StageContext`, `select`, `decide`, tier → target |
| `src/routing/stage/vocabulary.rs` | Claude Code tool names → categories |
| `src/routing/stage/signals.rs` | Anthropic Messages JSON → `ToolSignals` |
| `src/routing/stage/store.rs` | Session pins and hysteresis |
| `src/routing.rs` | `resolve_chain`'s router arm and the `Route.model` re-stamp |
| `src/proxy/failover.rs` | The single live call site |

Scoring itself is `switchyard-libsy` 0.2 and is not reimplemented. The version is
pinned to the `0.2` line and held out of automatic upgrades: 0.2.0 is the only
published release and upstream `main` has already diverged from it — `ToolSignals`
carries `tool_result_count`, `assistant_turn_count`, `new_count`, and
`recent_new_count` there, and 0.2.0 has none of them. Read the registry source,
not GitHub, when checking the API.

## 3. Signal extraction

Two passes over `messages`:

1. Walk assistant messages, building `tool_use.id → tool_use.name` plus a per-turn
   id list. `recent` is the set of ids from the last `recent_turn_window`
   assistant turns.
2. Walk user messages' `tool_result` blocks, join each back to its call by
   `tool_use_id`, and read `is_error` **off the result**.

**A `tool_result` block does not carry the tool's name.** The `tool_use_id` join
is the single point the extractor stands on, and it has its own test
(`joins_a_result_back_to_its_call`). `is_error` living on the result rather than
the call has another (`errors_are_read_from_the_result_not_the_call`).

Returns `None` when no completed call exists, which the caller treats as
`no_signal` rather than as agreement.

Two fields are deliberately left `false`, with the reasoning in the source:

- `tests_passed` would need result-*text* matching, and it pushes hard toward the
  cheap tier — guessing it wrong is expensive in exactly the wrong direction.
- `compacted` needs a compaction marker no test here pins yet.

### 3.1 Vocabulary

`Observe`: `Read`, `Glob`, `Grep`, `NotebookRead`, `WebFetch`, `WebSearch`,
`BashOutput`. `Mutate`: `Edit`, `NotebookEdit` (edit-shaped), `Write`
(write-shaped). `Plan`: `TodoWrite`, `Task`, `EnterPlanMode`, `ExitPlanMode`.
Everything else, `Bash`, `Skill`, `KillShell` and `mcp__*` included, is
uncategorised — which in 0.2.0 means it contributes through `pure_bash_streak`
rather than through a `new` counter, since that counter does not exist in the
published scorer.

Matching is `eq_ignore_ascii_case`. `Bash` stays uncategorised on purpose: only
`input.command` separates `git status` from `rm -rf` from `cargo test`, and
classifying command strings is a new fingerprinting surface with its own failure
mode. A future `tool_semantics` override is the intended door, not a default.

A failed `Task` scores severity 1.0 against 0.7 for anything else: it is a whole
delegated subagent run collapsing, not one command failing.

## 4. Hysteresis

`StageRouterStore` keys on `(advertised model id, SHA-256 prefix of the session
id)` — the session id is hashed and never stored, matching
`accounts::stable_session_index`.

| Situation | Behavior |
|---|---|
| No session id (absent or blank) | Decided statelessly; the store is not touched |
| `read_only` (count_tokens) | Decided, not recorded |
| Rejected before admission | Decided, not recorded |
| Efficient → Capable | Immediate on any scorer-made decision |
| Capable → Efficient | `dwell_turns >= min_dwell_turns` **and** confidence `>= deescalate_threshold` **and** a scorer-made decision |
| `fall_open` / `no_signal` / `ambiguous` | Cannot move a pin in either direction |
| Fingerprint mismatch or TTL expiry | That entry alone is treated as absent |

`tests_passed` is exempt from the *confidence* gate but not the dwell one. It is
libsy's hard de-escalation shortcut, which skips the scorer and returns
`confidence: None` (`resolved(Efficient, TestsPassed, 0.0, None)`); a bare
`confidence >= threshold` gate would make the strongest reason to go cheap the
one reason that could never fire.

That exemption is written against libsy's contract, not against a signal shunt
produces today: §3 leaves `tests_passed` unconditionally `false`, so
`DecisionSource::TestsPassed` cannot be returned and the exemption is inert. It
is kept so that populating the signal later is a change to the extractor alone
(ADR-0004).

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

Capacity: `MAX_TRACKED_SESSIONS = 4096`, one amortised O(n) pass on insert that
drops TTL-expired entries first and then the oldest. No timer, no background task,
nothing persisted. `apply` takes `now: Instant` so dwell, expiry, and eviction are
tested against a clock the test moves.

### 4.1 Why the asymmetry exists

A tier flip forfeits the per-model prompt-cache prefix; it forces
`ContinuationOutcome::Fallback` because `codex_continuation::signature` hashes
`model`; and across providers it abandons the pooled socket and sticky account
slot. Scored independently every turn, a long session can cost more routed than
unrouted. Anything that makes de-escalation cheaper must re-derive this.

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
a fresh session starts on.

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

**Metrics.** `shunt.stage_router.decisions` counts decisions by requested
model, tier, and source. `shunt.stage_router.flips` counts those that moved a
session off its pinned tier, `from` and `to` kept separate because §4.1's
asymmetry means the two directions are not expected to match. The decision
counter cannot show churn on its own: a session pinned to `capable` and one
that just arrived there are the same row. Every label is a closed set, so the
series count is bounded by the number of configured routers rather than by
session count.

**`GET /routes`.** Grows a `routers` array — `model`, `capable_target`,
`efficient_target`, `default_tier`. A router's targets need not have
`[[routes]]` entries, so `data` alone never named them. The field is omitted
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
