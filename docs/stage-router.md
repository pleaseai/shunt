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

Out of scope, deliberately: the LLM classifier, mid-turn escalation, the advisor,
and `[server.codex_endpoint]` (which uses its own routing table and never reaches
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
(write-shaped). `Plan`: `TodoWrite`, `Task`, `Skill`, `ExitPlanMode`. Everything
else, `Bash` and `mcp__*` included, is uncategorised — which in 0.2.0 means it
contributes through `pure_bash_streak` rather than through a `new` counter, since
that counter does not exist in the published scorer.

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
| No session id | Decided statelessly; the store is not touched |
| `read_only` (count_tokens) | Decided, not recorded |
| Efficient → Capable | Immediate on any scorer-made decision |
| Capable → Efficient | `dwell_turns >= min_dwell_turns` **and** confidence `>= deescalate_threshold` **and** a scorer-made decision |
| `fall_open` / `no_signal` / `ambiguous` | Cannot move a pin in either direction |
| Fingerprint mismatch or TTL expiry | That entry alone is treated as absent |

`tests_passed` is exempt from the *confidence* gate but not the dwell one. It is
libsy's hard de-escalation shortcut, which skips the scorer and returns
`confidence: None` (`resolved(Efficient, TestsPassed, 0.0, None)`); a bare
`confidence >= threshold` gate would make the strongest reason to go cheap the
one reason that could never fire.

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

A target matching no explicit route is a **warning**, not an error: resolution
always falls back to `server.default_provider`, so such a target is reachable and
rejecting it would be wrong.

## 7. What is not built

Metrics (`record_stage_decision`, `record_stage_flip`), the
`x-gateway-routed-model` / `x-gateway-route-source` response headers, and
`RoutesResponse.routers` are a follow-on change. Until they land, "why did this
turn use that model" is answerable only by reading the request.
