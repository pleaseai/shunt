# Routing algorithms

Engineering note for the work in
[ADR-0005](../.please/docs/decisions/0005-routing-algorithm-extensions.md):
full Switchyard routing-algorithm parity on `[[models]]`. It grows one section
per PR in the ADR §8 sequence.

[`stage-router.md`](stage-router.md) remains the record for the signal-only
stage router that shipped under ADR-0004. This note covers the dependency the
rest of the algorithms are built on, and, as they land, how each is hosted.

## 1. The libsy dependency (PR 0)

### What it is

```toml
switchyard-libsy = { git = "https://github.com/NVIDIA-NeMo/Switchyard", rev = "3ddea9d30174ad835cf505a93617ba251c8eb8dd" }
```

A `rev` and **no `branch` key** — the same form as the two `tungstenite` pins in
the `[patch.crates-io]` table below it. That is the whole point of the shape: a
branch pin moves under an unrelated `cargo update`, a `rev` pin does not, so a
bump is a reviewed diff with a commit message.

### Why a git pin rather than the release

`switchyard-libsy` 0.2.0 is the only published version and exports only the
stage scorer. Upstream `main` (`0.3.0`-pre) exports `AdvisorGate`,
`CompositeRouter`, `SubagentRouter`, `ToolSemantics`, and `HandoffNoteConfig` —
every algorithm past §1 of the ADR. shunt is `publish = false`, so a git
dependency is available to it; that was the fact that changed between ADR-0004
and ADR-0005 (ADR-0005 §9).

Moving from the release to the pin added **no packages**: the lock stayed at 445
entries, because libsy's new direct dependencies there (`http`, `regex`, `uuid`)
were already in shunt's tree. The `jsonschema` tree is still not linked — zero
`jsonschema` strings and zero `num-bigint` symbols in the release binary, the
same check ADR-0004 used to justify taking the dependency at all.

### Reading the API

Read the checkout cargo resolved:

```
~/.cargo/git/checkouts/switchyard-<hash>/<short-rev>/crates/libsy/src/
```

Not the registry source (that is 0.2.0, a different API) and not GitHub's
default branch (it moves; the pin does not).

### Bumping the rev

1. Pick the new commit and read `crates/libsy/src/algorithms/util/` against the
   current one — `stage.rs` and `tool_signals.rs` are the two files shunt's call
   path depends on.
2. Change the `rev` in `Cargo.toml`, run `cargo fetch`, and check the
   `Cargo.lock` diff for packages the bump adds.
3. `cargo test --all-features --workspace`, then the stage-router bench with
   `--features bench`.
4. Re-point every hard-coded copy of the rev — this note's snippet above and
   the `[libsy]` link definitions in the site guides (`switchyard.mdx` and
   `stage-router.mdx`, English plus `ja`, `ko`, `zh-cn`). `git grep <old
   rev>` must come back empty: those links are pinned for the same reason
   the dependency is, so one left behind sends readers to source the build
   does not use.

Two things in `src/routing/stage.rs` are deliberately built so an upstream
change fails the build rather than drifting silently: `StageSource::Scorer`
carries libsy's own `DecisionSource` instead of re-spelling it, so a new variant
breaks `is_signal_evidence`'s match; and `signals::extract` names the
`ToolSignals` fields it decides rather than relying on the struct's shape.

### What the pin changed

| Upstream change | Effect here |
| :-- | :-- |
| `ToolSignals` gained `repeated_failure`, `tool_result_count`, `assistant_turn_count`, `new_count`, `recent_new_count` | The two counters are filled from the walk the extractor already does; the other three stay at zero (`stage-router.md` §3) |
| `DecisionSource::TestsPassed` removed | The hard de-escalation shortcut is gone upstream; its confidence-gate exemption went with it. It was inert here — the extractor pins `tests_passed` to `false` — so no decision changes |
| `DecisionSource::CapableHold` added | Not signal evidence: it is an earlier escalation being held, which is what `StageSource::Sticky` already means, and only libsy's stateful `StageClassifier` stamps it. shunt calls `pick_tier` directly |
| `PickOutcome`'s `score` renamed to `probability` | Not read here — both arms already destructure with `..` |
| Confidence gate is now a band around the midpoint | `probability` outside `[0.5 − t/2, 0.5 + t/2]` is arithmetically `|score| > t`, against `>= t` before. A turn scoring *exactly* at the threshold now falls open |

`score_signal` is byte-identical to 0.2.0's, so the calibration
`docs/stage-router.md` and the site's Switchyard page describe is unchanged.

### Not in this PR

Beyond the exact-threshold reclassification the table above notes, PR 0
introduces no new routing behaviour, no config key, and no new code path, so
it adds no benchmark arm: the one function it touches, `signals::extract`, is
already an arm of `benches/stage_router.rs`. The lanes, the
`[models.router]` discriminator, and the driven algorithms are PR 1 onward — see
ADR-0005 §8 for the sequence and each step's definition of done. PR 1 is §2
below, PR 2 is §3, PR 3 is §4, PR 4 is §5, PR 7 is §6, and PR 5 is §7; the rest
of the sequence is not implemented yet.

## 2. Request hints, the pin scope, and the compaction latch (PR 1)

### What it is

`src/routing/context.rs` reads five inbound headers into a `RouterContext`,
built once per request and stored nowhere:

| Header | Read as |
| :-- | :-- |
| `x-claude-code-session-id` | The session the pin is keyed on |
| `x-claude-code-agent-id` | The delegated agent the pin is scoped to |
| `x-claude-code-request-class` | `main`, `subagent`, `workflow`, `compaction`, or `auxiliary`; an unrecognised value reads as absent |
| `x-claude-code-agent-type` | The `by_type` key of the `[models.subagents]` overlay (§4); a `by_type` map keys on the literal value |
| `x-claude-code-context-compacted` | Read by presence of a non-blank value (`manual`, `auto`, `reactive`) |

Nothing is percent-decoded: the class is compared against ASCII literals an
encoded byte can never equal, and the two ids are only ever hashed, where the
encoded form is as stable across turns as the decoded one.

### Why the agent id and not the class

Claude Code gates four of the five headers client-side — behind any
non-Anthropic base URL, which is every shunt deployment, they are off until the
operator sets `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`. `x-claude-code-agent-id` is
not gated. So `RouterContext::is_delegated` reads the class when it is there
(`subagent` and `workflow` are delegated; `main`, `compaction`, and `auxiliary`
are not) and otherwise falls back to "a non-blank agent id means delegated
work" — which is the live path on every default deployment, and is what makes
child pins work with no operator action. What the gate additionally unlocks is
the class-authoritative rule and the compaction latch.

`main` carrying an agent id is parent traffic under that rule. The combination
has never been observed on the wire, so the branch is defensive: it keeps the
header that *names* the class ahead of the one that merely correlates with it.

### What changed

| Change | Effect |
| :-- | :-- |
| The pin key gained a third element | `(advertised model id, sha256(session id)[..16], sha256(agent id)[..16])`, all zeros for a parent. A `Task` child reads and writes its own pin, so its failures no longer escalate the parent and its turns no longer advance the parent's dwell (ADR-0005 fact 4) |
| A second eviction budget | `MAX_TRACKED_CHILD_PINS = 4096` beside `MAX_TRACKED_SESSIONS = 4096`. A commit trims only the scope it grew, against that scope's cap, so a wide fan-out cannot evict the parents waiting on it. The TTL sweep stays scope-blind |
| The pin carries `compacted` | The turn sending `x-claude-code-context-compacted` is scored with `ToolSignals.compacted`, reaching libsy's hard override — `capable`, source `override` — even with no tool activity in the transcript. The flag is latched onto the pin, so the turns after it (the header is one-shot) resolve the same way. It clears with the pin: TTL expiry, or a reload that changes the router table |
| `GET /protocol` | `request_headers.consumed` now lists `x-claude-code-request-class`, `x-claude-code-agent-type`, and `x-claude-code-context-compacted` beside the session, agent, and parent-agent ids |
| Two benchmark arms | `store_turn_new_child_at_capacity` (the child-budget twin of `store_turn_new_session_at_capacity`) and `resolve_chain_routed_delegated` (the routed path under a child's headers, read against `resolve_chain_routed`) |

`docs/stage-router.md` §4 is the implementation record for the key, the budgets,
and the latch; §4.2 covers the latch specifically.

### Not in this PR

The `[models.subagents]` `by_type` map is PR 3 (§4). The read-only carve-out
ADR-0005 §11 describes for the `compaction` and `auxiliary` classes is a later
step in the §8 sequence. `x-claude-code-compaction` and
`x-claude-code-prev-tool-durations` are read by nothing and are therefore
deliberately absent from `/protocol`'s consumed list.

A request carrying none of these hints — a bare `curl`, a Claude Code older than
the headers, or the default gate-off deployment with no agent id — routes
exactly as it did before.

## 3. The `[models.router]` discriminator and the pure lane (PR 2)

### What it is

`[models.stage_router]` is now `[models.router]`, a single table carrying a
`type` discriminator. Four types land here — the **pure lane**, the algorithms
that decide with no judge call and so stay on the synchronous path ADR-0005 §1
describes:

```toml
[[models]]
id = "claude-auto"
[models.router]
type = "stage_router"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
picker = "efficient_first"           # or "capable_first"
confidence_threshold = 0.5
recent_turn_window = 3
min_dwell_turns = 3                  # shunt's hysteresis keys stay
deescalate_threshold = 0.75
session_ttl_seconds = 3600
capable_hold_turns = 0               # upstream key; shunt's default is 0, upstream's is 2
[models.router.tool_semantics]       # optional; four lists
observe = ["mcp__jbcontext__code_search"]
mutate = []
plan = []
new = []
[models.router.handoff_notes]        # optional
escalation_note = "the previous model was stalling; pick up the diagnosis"
deescalation_note = "routine work resumes"   # optional
only_on_wrong_signal_escalation = true       # default

[[models]]
id = "claude-quick"
[models.router]
type = "auto"                        # upstream's stage-router preset
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"

[[models]]
id = "claude-canary"
[models.router]
type = "random"
targets = ["claude-sonnet-4-6", "gpt-5.6-terra"]
weights = [9, 1]                     # optional; equal when omitted, and a zero disables a target
                                     # each weight finite, and their sum finite too
seed = 0                             # optional
affinity = "session"                 # shunt addition; "session" (default) or "request"

[[models]]
id = "claude-noop"
[models.router]
type = "noop"                        # no other keys
```

`router` and `upstream_model` on one entry are mutually exclusive, as
`stage_router` and `upstream_model` were.

### Why `type`, and why no alias

`type` is a deliberate exception to shunt's `kind`/`mode` convention, taken so
upstream's own `type` values and its schema pages quote unchanged (ADR-0005 §2,
settled in §9). The exception is **stated once**, on the site's configuration
reference; the other pages just use the key.

ADR-0005 §2 planned `[models.stage_router]` as a deprecated alias warned once
per load. It is not one. No release tag contains the commit that added that
table, so there is no deployed configuration the alias would keep loading —
only branch builds, which are migrated by editing the line. `[models.router]`
is therefore the only spelling: a config still writing `[models.stage_router]`
**fails to load**, with an error naming the new form, and no deprecation
warning exists to emit. ADR-0005 §2 carries the dated amendment.

### What changed

| Change | Effect |
| :-- | :-- |
| `[models.router]` with `type` | One table hosts every algorithm. `stage_router` keeps every key it had, so a rename of the table header is the whole migration |
| `type = "auto"` | Upstream's preset: `stage_router` with `picker = "efficient_first"` and `confidence_threshold = 0.5`, the rest at shunt's defaults. Only `capable_target` and `efficient_target` are accepted beside it; its route sources are the stage router's |
| `type = "random"` | A weighted split across `targets`, session-sticky by hash under the default `affinity = "session"` (below) |
| `type = "noop"` | No upstream call at all: an empty terminal assistant message in the caller's mode (below) |
| The one-hop rule generalises | Any target of any router type that resolves, after a trailing `[1m]`/`[1M]` is stripped, to a `[[models]]` entry must resolve to one that carries no `router`. A target naming no entry still falls through `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider`, and warns once at load — the existing unresolvable-target warning, now emitted for every router type |
| Targets stay public model ids | A chosen target re-enters the ordinary ladder and inherits failover, pools, adapters, `effort`, and `service_tier` (ADR-0005 §2). Nothing about that is new; it now holds for four algorithms instead of one |
| `tool_semantics` | Four operator lists over the built-in Claude Code vocabulary (below) |
| `handoff_notes` | A system block appended on a signal-driven tier change (below) |
| `capable_hold_turns` | After a signal-driven escalation the capable tier is held for N further turns, reported as route source `capable_hold`. A hold is **not** evidence and cannot move a pin, so the shunt default of `0` leaves pins behaving exactly as they did. `count_tokens` probes neither set nor consume a hold. libsy's early clear on a passing test never fires: shunt leaves `tests_passed` unextracted (`stage-router.md` §3) |
| One new metric | `shunt.router.decisions{model, algorithm, target, source}`, emitted for every router type including stage. `shunt.stage_router.decisions` and `.flips` are unchanged |
| `x-gateway-routed-model` / `x-gateway-route-source` | Stamped for every router type, not only the stage router |
| `GET /routes` | Each `routers[]` entry gains `algorithm` and `targets`. `capable_target`, `efficient_target`, and `default_tier` stay, and are present for `stage_router` and `auto` only |
| One new benchmark arm | `resolve_chain_random_session`. `resolve_chain_unrouted` stays flat — the router arm is still one `Option` check (`stage-router.md` §9) |

### `random`: the arm is a hash, not a stored choice

Under `affinity = "session"` the arm is `sha256(seed ‖ model ‖ session_id)`
scaled into the weight range. There is no store: one Claude Code session keeps
one arm across restarts, and across replicas that load the same config, because
the arm is a pure function of those three inputs. `seed` is the hash salt and
is `0` when unset; changing `seed`, `targets`, or `weights` can move the arm,
which is the whole cost of the no-store design.

A request carrying no non-blank `x-claude-code-session-id` takes a **fresh
weighted draw** instead. The blank id is never hashed as a shared key, so a
90/10 split stays 90/10 for sessionless clients rather than collapsing every
one of them onto whichever arm the empty string hashes to.

Under `affinity = "request"` every request draws, and with `seed` set the draw
sequence is reproducible — upstream's own meaning for the key.

Session affinity is **stickiness, not access control** (ADR-0005 §2): the
session id is client-supplied, so a caller that retries ids can steer itself
onto the arm it wants. It guards nothing, because the targets are public model
ids that same caller may simply name.

Body-less surfaces — `GET /routes`, discovery, `shunt check` — have no session
to hash and report the **first positive-weight target**, the same way they
report the picker default for a stage router. Route sources are `random` for a
fresh draw and `random_session` for a hash-pinned one, so the two are readable
apart in the header and in the metric.

### `noop`: a well-formed empty turn

`noop` makes no upstream call. It synthesises an empty terminal assistant
message in the caller's own mode: for `stream: true`, a valid SSE sequence —
`message_start`, `message_delta` carrying `stop_reason: "end_turn"`, then
`message_stop`; otherwise one Message JSON object. `count_tokens` answers
`input_tokens: 0`. The route is still gateway-authenticated exactly like any
credential-injecting route, so a `noop` entry is not an unauthenticated hole.
Its route source is `noop`. It exists for smoke tests and client wiring checks:
it proves the whole inbound path — auth, admission, adapter, stream shape —
without spending a token.

### `tool_semantics`: an overlay, not a replacement

Four lists — `observe`, `mutate`, `plan`, and `new`; the pinned libsy carries
`new_count`, which is why the fourth is accepted at all (§1). They are applied
**after** the built-in Claude Code table, and a name that table already
classifies as observe, mutate, or plan (`Read`, `Edit`, `TodoWrite`, …) is
**rejected at load** rather than silently overridden. The intended use is the
names the table deliberately leaves uncategorised — `Bash`, `Skill`, and
`mcp__*` server tools (`stage-router.md` §3.1). An operator `mutate` name is
scored as a whole-file write.

A name carrying whitespace is rejected at load too. Matching is an exact
`eq_ignore_ascii_case`, so `" Read "` is neither blank nor a recognised built-in:
it would clear both of the checks above and then match nothing. Rejecting beats
trimming — an operator who typed a space meant some name, and guessing which is
how the config and the runtime drift apart again.

The lists join the store fingerprint, so a reload that changes them drops the
pins made under the old ones — the same rule every other router key follows.

### `handoff_notes`: the cost is a cache miss

On a signal-driven escalation to the capable tier — route sources `override`
and `dimensions`, or every scorer decision when
`only_on_wrong_signal_escalation = false` — `escalation_note` is appended as a
**new system block at the end of the forwarded `system` array**. On a
scorer-driven hand-back to the efficient tier, `deescalation_note` is, when it
is set. Sticky and no-signal turns carry no note, and neither does
`count_tokens`.

Both notes require the turn to actually **move** the tier. Two turns look like
escalations and are not: the first turn of a session, which has no earlier tier
to explain, and a later signal that merely re-confirms the tier already pinned —
which keeps its `dimensions` source and so is indistinguishable from the signal
that first earned the tier. `RouterOutcome.handed_off`, not the route source, is
what separates them.

The Claude Code attribution block is the first system block and is left
untouched (ADR-0005 §6): the note is a separate block after it, never an edit
to it.

The cost, and the reason the notes are opt-in: **every toggle is a
prompt-cache miss.** The system array is part of the cached prefix, so
appending or dropping the suffix invalidates it — on top of the per-model
prefix a tier flip already forfeits (`stage-router.md` §4.1).

### Not in this PR

The driven lane is untouched by PR 2: `llm_classifier`, `composite`, `advisor`,
and `stage_router.classifier` all make judge calls, so a `type` naming one of
them is a load error rather than a silent pass-through. Of those,
`stage_router.classifier` has since shipped — it is PR 4, §5 below, and with it
the `shunt.router.judge_calls` metric and the `judges` list on `GET /routes`
that describe judge traffic (ADR-0005 §7). `llm_classifier` and `composite`
have since shipped too — PR 5, §7 below — leaving `advisor` and
`llm_classifier`'s `escalation` mode as the load errors. `prefill_router` is PR 7, behind
its own cargo feature, §6 below. `[models.subagents]` is PR 3, §4 below.

A request to an id whose `[[models]]` entry carries no `router` table routes
exactly as it did before, and pays the same one `Option` check it paid before.

## 4. The `[models.subagents]` passthrough overlay (PR 3)

### What it is

`[models.subagents]` diverts **delegated work** — a `Task` child, a hook
agent, a workflow sub-agent — away from the destination the entry gives its
parent. It sits on the `[[models]]` entry, beside `upstream_model` or
`router`, not inside the router table: a fixed entry has no router table, and
upstream's "passthrough with subagents" is exactly a fixed entry here. Only
the `passthrough` form lands in this PR, on the pure lane:

```toml
[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic = "claude-opus-4-8"
[models.subagents]
type = "passthrough"
target = "claude-haiku-4-5"          # every delegated turn `by_type` does not name
by_type = { Explore = "claude-haiku-4-5", fork = "claude-sonnet-4-6", teammate = "claude-sonnet-4-6" }
```

`src/config/subagents.rs` is the table; `src/routing/subagents.rs` is the one
function that answers it, `select(overlay, hints) -> Option<(target, source)>`,
and `resolve_chain` consults it before the entry's router or map. `None` — the
turn is not delegated — falls through to the arms that existed before, so the
parent's own turns are untouched by the table's presence.

### Why the class wins, and why the fallback exists

Delegation is `RouterContext::is_delegated` (§2): `subagent` or `workflow` when
`x-claude-code-request-class` is sent, else a non-blank `x-claude-code-agent-id`.
The class is authoritative when sent. `main` with an agent id is main traffic —
never observed on the wire, but the header that *names* the class must beat the
one that merely correlates with it. `compaction` and `auxiliary` are harness
maintenance and never take the overlay, for the reason upstream's
`SubagentOverride` abstains on `compact`: a title-generation call or the compact
call itself is not the child's work, and diverting it would put the session's
own maintenance on the child's tier.

The fallback exists because the class is gated client-side and the agent id is
not (§2). Behind a default shunt deployment every `Task` child therefore takes
`target`, and `by_type` — which needs `x-claude-code-agent-type` — is inert
until the operator sets `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` on the client.

`by_type` keys are the header's **literal** values, matched exactly
(`docs/notes/adr-0005-routing-live-captures.md`, fact (b)): built-in ids travel
verbatim, case included (`Explore`, `Plan`, `general-purpose`, `claude`; `fork`
only under `CLAUDE_CODE_FORK_SUBAGENT=1`), a project agent arrives as `custom`
with its name withheld, and `teammate` is the binary's literal, not yet seen
live. A key with whitespace is rejected at load rather than trimmed — it could
never match, and the operator meant some type.

### What changed

| Change | Effect |
| :-- | :-- |
| `[models.subagents]` with `type = "passthrough"` | `target`, plus the optional `by_type` map. Rides a fixed entry, a router-backed one, or a map-less id |
| The overlay runs before the router | A child requesting a `stage_router` id is diverted before `stage::select`: its transcript is never scored, no pin is parked, and its parent's dwell is never advanced by it. No store access on the path |
| The one-hop rule covers the overlay | `target` and every `by_type` value must not resolve, after the `[1m]` strip, to an entry carrying a `router` **or** a `subagents` table; and a router target may not resolve to an entry carrying an overlay. Both directions are rejected at load |
| Load-time rejections | A blank target (named by key: `target`, `by_type.Explore`), a blank or padded `by_type` key, an overlaid id ending in `[1m]`/`[1M]`, a duplicate `[[models]]` id where either entry carries the table, and the `llm_classifier` form (by the enum's unknown-variant error) |
| The unresolvable-target warning | Ranges over overlay targets too, with the same "falls back to the default provider" message |
| Two route sources | `subagent_type` for a `by_type` hit and `subagent` for the `target` fallback — the same split `random`/`random_session` makes, so the header and the metric say which key decided. `algorithm` is `subagents` |
| `x-gateway-routed-model` / `x-gateway-route-source` | Stamped on a diverted turn like any router decision; absent on the parent's turns through the same id |
| Body-less surfaces | No headers, and none of them resolve the diverted target: `/v1/models` discovery and `shunt check` carry no per-model destination at all, and `GET /routes` shows the parent's own `[[routes]]`/`[models.router]` entry only when one exists — nothing for an id left to `server.default_provider`. The overlay is not listed in `/routes` `routers[]` |
| One new benchmark arm | `resolve_chain_subagents_passthrough`: the child's turn from `resolve_chain_routed_delegated`, once the routed entry also carries an overlay. Read against that arm — the gap is the scoring and the store read the child stops paying — and flat across turn counts |

### Not in this PR

The `llm_classifier` form of the overlay is PR 5 with the rest of the driven
lane. The read-only carve-out for `compaction` and `auxiliary` on the stage
router — landing them on the session's tier without recording — is still a
later step; absent the overlay they route as they do today, and with it they
route as the parent does. `GET /routes` does not yet describe the overlay.

## 5. Dependency envelope, admission before drive, internal serve, per-call bounds (PR 4)

### What it is

The first entry on the **driven lane**: a `type = "stage_router"` that carries a
`[models.router.classifier]` table. The table names a judge — a public model id
consulted for a verdict and never served to the client — and the six per-call
bounds that hold every internal call shunt makes on the caller's behalf.

```toml
[[models]]
id = "claude-auto"
[models.router]
type = "stage_router"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
judge_timeout_ms = 30000              # the six bounds, shown at their defaults
judge_max_response_bytes = 65536
gated_max_bytes = 8388608
gated_idle_ms = 60000
gated_max_duration_ms = 600000
max_judge_calls = 8
[models.router.classifier]            # optional; adding it moves the entry to the driven lane
target = "claude-haiku-4-5"           # consulted, never served
base_threshold = 0.5
```

`[models.router.classifier]` is accepted on `type = "stage_router"` only.
`type = "auto"` keeps its two-key wire shape and rejects the table, so the
preset stays exactly the preset.

The judge is consulted on a turn the signal scorer leaves undecided — today's
`fall_open` turns — on a session no pin is holding. It is never consulted on a
`count_tokens` probe, and never before admission. A judged turn's route source
is `llm-classifier`; a judge failure of any kind resolves as `fall_open`, the
picker default. The verdict pins the session the way any other first decision
does.

| Module | What it holds |
| :-- | :-- |
| `src/config/router/bounds.rs` | `CallBounds`, the six `DEFAULT_*` values, and the "at least 1" check |
| `src/routing/envelope.rs` | `dependency_envelope` — the routes admission ranges over |
| `src/routing/serve.rs` (with `serve/bounds.rs`) | `AdmittedContext`, the `judge_call` dispatch, and the bounded collectors |
| `src/routing/judge.rs` | The `drive` step: neutral request in, `Decided`/`FailOpen` out |
| `src/proxy/failover/chain.rs` | The attempt loop, extracted so client traffic and the judge dispatch share one code path |

### Why admission moves ahead of the chain

`proxy::failover` resolves the chain before it authenticates, because the auth
gate needs the resolved chain to know whether a route injects a credential. A
driven entry cannot keep that order: it would spend judge calls and
gateway-held credentials deciding a turn for a caller who is about to be
rejected. So a driven entry's **dependency envelope** is computed first, with
no model call — the requested id, plus every answer target and the judge target,
each contributing its *whole* failover chain through the ordinary ladder
(`[[models]]`, then `[[routes]]`, `[[route_prefixes]]`, `server.default_provider`).
An alias mapping several ordered upstreams contributes all of them, and a judge
naming no entry at all is a member carrying `server.default_provider`'s
credential behaviour rather than an absent one.

Admission then runs against that union, in this order:

1. resolve the requested id's own chain, as before;
2. compute the envelope — only for a turn that will actually consult a judge,
   so an unrouted id allocates nothing new here and neither does a driven entry
   whose turn reached no judge;
3. `check_inbound_auth` against the envelope: the caller must authenticate if
   **any** member injects a credential, so a passthrough answer target with a
   credential-injecting judge is not a passthrough route;
4. enforce the managed-model policy on the **requested** public id;
5. `drive`.

The consultation selects the envelope, not the entry's static shape. A driven
entry resolves plenty of turns without a judge: the signals decide most of
them, and a `[models.subagents]` overlay answers a delegated turn before the
router runs at all (§4). Gating those against the envelope would demand a token
for a credential the request cannot reach — the chain it was actually routed to
injects nothing, so the refusal names a dependency that turn does not have.
Ordering is unaffected: `stage::select` sets the consultation while the chain
is resolved, which is already before admission, so the judge still cannot run
for a caller who is about to be refused.

`count_tokens` is unchanged: it keeps first-route-only admission, never enters
`drive`, and makes zero judge calls. A passthrough answer target with an
injecting judge therefore still answers an unauthenticated probe exactly as it
does today.

The `prefill_router` lane (§6) takes the same path, with local inference in
place of the judge call (issue #633). A turn whose id is a prefill entry is
resolved to the entry's default target provisionally and admitted against the
envelope — every request, `count_tokens` included, because that lane drives
probes too — and `decide` runs only after both gates. The rule the two lanes
share is that a turn which *will* drive is gated against the envelope; the
judge lane exempts probes only because it never drives them.

`serve` receives a non-forgeable `AdmittedContext`, minted only on that gate's
success path. A judge call without one is a bug, not a request.

### Why the judge carries none of the caller's credentials

`proxy::failover` computes a passthrough origin from the first route of the
chain it is handed, which for an internal call is the judge's own route — so a
caller credential presented for the answer provider would be forwarded to
whatever provider the judge maps. The judge call therefore starts from the
caller's headers with every credential slot removed:
`ShuntCredentials::strip_reserved_slots` takes the reserved `x-shunt-*` and
`cookie` slots, then both `SHARED_SLOTS` names (`authorization` and
`x-api-key`) are removed **unconditionally** — not through
`strip_consumed_slots`, which deliberately keeps a caller's genuine upstream
credential and would keep it here too — and `anthropic-beta` goes with them.

The call runs on whatever credential its target's route injects, which is why
a judge target that resolves to a **passthrough** route has nothing to run on
and is rejected at validation. `auth = "none"` is not that case and is
accepted: `route_is_passthrough` tests `AuthMode::Passthrough` alone, and an
endpoint that needs no credential is not one whose credential went missing — a
local or self-hosted judge behind no auth is a supported configuration. The
injecting case also means judge calls consume that target's pool quota: a judge
should map its own `[[models]]` entry.

### The wire shape of the judge request

What the codec emits, read off the pinned revision and confirmed on the wire
(`docs/notes/adr-0005-routing-live-captures.md`, "Fact (c), re-captured"): one
**string** `system`, `max_tokens` `4096`, and
`output_config.format = {"type": "json_schema", "schema": …}` — `schema`, not
`json_schema`, with `minimum`, `maximum`, `minLength`, and `maxLength`
recursively stripped from it. No `tools`, no `tool_choice`, no `stream`, no
`anthropic-beta`.

Neutral ⇄ Anthropic conversion at this boundary is
`switchyard-translation`'s `anthropic_messages` codec, in both directions —
the request and the judge's reply. The Anthropic → Responses wire translation
stays shunt's own (`src/model/responses_request.rs`). Two translators, two
jobs.

### What changed

| Change | Effect |
| :-- | :-- |
| `[models.router.classifier]` | `target` (required) and `base_threshold` (default `0.5`, in `(0.0, 1.0]`) — the lowest `p_solve` that keeps a supported task on the efficient tier. `stage_router` only |
| Six per-call bounds on `[models.router]` | `judge_timeout_ms` `30000`, `judge_max_response_bytes` `65536`, `gated_max_bytes` `8388608`, `gated_idle_ms` `60000`, `gated_max_duration_ms` `600000`, `max_judge_calls` `8`. Each must be at least `1`; a `0` is a startup error naming the key. Crossing a bound cancels the upstream call |
| `judge_timeout_ms` is end-to-end | Headers *and* body, on the non-streaming judge call. A `.send()`-only timeout stops at headers, and a `200` that then stalls would never reach `fail_open` |
| `gated_idle_ms` and SSE pings | Measured between *completed content frames*, not between chunks: a frame split across chunk boundaries is reassembled from a carried remainder before it is classified, so an endless keep-alive stream cannot disarm the bound by splitting `event: ping` mid-line. A chunk that completes no frame resets nothing. Frames are split after normalizing CRLF and bare-CR line endings, so a CRLF chunk carrying real content beside a keep-alive still counts as progress |
| `judge_max_response_bytes` bounds the allocation | Enforced where the adapter reads the upstream body, not on what reaches the JSON parser. A judge route is an alias route by construction, so its reply takes the adapter's buffered branch; capping only afterwards would spend the memory the bound exists to deny. Client traffic is uncapped and unchanged |
| `max_judge_calls` | Per session, counted under the pin |
| Admission on the envelope | Inbound auth ranges over the requested id plus every answer and judge target's full chain; the managed-model policy stays on the requested id |
| Judge credentials | Reserved slots plus `authorization` and `x-api-key` stripped unconditionally, `anthropic-beta` with them; a passthrough judge target is a startup error |
| `shunt.requests` / `shunt.latency` | Gain a `caller` attribute, `client` or `router` |
| One new counter | `shunt.router.judge_calls{model, algorithm, outcome}`, with the closed outcome set `decided`, `timeout`, `oversized`, `upstream_error`, `invalid_reply`, `translation_failed`, `budget_exhausted` |
| `GET /routes` | Each `routers[]` entry gains `judges`, omitted when empty — the ids consulted but never served |
| `x-gateway-route-source` | Gains `llm-classifier` for a judged turn. The scorer's `ambiguous` still cannot reach a response |
| One new benchmark arm | `dependency_envelope_driven`, the envelope walk for a stage router carrying a classifier |

The `gated_*` keys are accepted, validated, and enforced by the retained-turn
collector, but **no gated turn exists yet**: the escalation weak turn and the
advisor executor turn are PR 6. Until then the three keys bound nothing at
runtime, and the collector is exercised by its own unit tests only. PR 6 (§8)
now enforces them on every gated turn.

### What is tested

Unit, alongside the code: the classifier table's defaults; each of the six
bounds rejected at `0`, naming the key; `base_threshold` out of range; a blank,
`[1m]`-suffixed, or router-typed `classifier.target`; a passthrough judge,
including one reached only as a later member of the judge's own chain and one
reached only by fall-through to a passthrough `server.default_provider`; a
`classifier` key on `type = "auto"`; the three envelope shapes; the judge
budget accumulating across commits and a verdict restarting dwell; `consult`
set on `fall_open` alone and never on a read-only probe or a sticky turn; the
four bounded-collector failures plus the ping-does-not-reset case and its CRLF
twin; an endless judge reply resolving as `oversized` rather than draining to
the deadline; the `caller` attribute and the judge counter through the metric
sample stores; and `judges` present on `/routes` only with a classifier
configured.

Integration, in `tests/router_judge.rs`, one focused test per clause: an
undecided turn is judged once and answers under `llm-classifier`; a decisive
turn makes no judge call; an invalid credential and a policy-denied model each
make none, on both the passthrough-answer + injecting-judge shape and the
fall-through shape; an unauthenticated `count_tokens` probe is answered with
none; the judge's headers carry the judge provider's injected key and nothing
of the caller's; the call lands on the judge's own provider and not on the
tier mocks; a `200`-then-stall and an endless ping stream each resolve as
`fall_open` within the deadline and close the upstream connection; and
`max_judge_calls` stops a session at its budget.

### Not in this PR

The rest of the driven lane: `llm_classifier` (capability and custom),
`composite`, and the `[models.subagents]` classifier form have since shipped as
PR 5, §7 below, and with them the `classify_trigger` key this PR's classifier
table gained. Buffer-and-replay — the escalation weak turn and the advisor
executor turn the `gated_*` bounds are built for — has since shipped as PR 6,
§8 below, with the streaming-semantics amendment `AGENTS.md` needed.
`[models.subagents]` is PR 3, shipped as §4 above, and `prefill_router` is
PR 7 (§6), behind its own cargo feature.

An entry that configures no `[models.router.classifier]` stays on the pure
lane: no judge, no envelope walk, and the same synchronous decision it made
before.
## 6. `prefill_router` behind the `prefill-router` cargo feature (PR 7)

### What it is

Upstream's learned router: a classifier over the latest text user turn picks
one of the entry's targets. The scoring model runs in-process — it is not a
judge call to an upstream provider — and the entry is configured the same way
in every build:

```toml
[[models]]
id = "claude-learned"
[models.router]
type = "prefill_router"
targets = ["claude-sonnet-4-6", "claude-opus-4-8"]   # public model ids, in the checkpoint's head order
checkpoint = "/models/router.pt"                     # tensor-only router checkpoint; a relative path resolves against the process cwd
device = "cpu"                                       # optional; auto-detected when omitted (cpu, cuda, cuda:0)
cache_dir = "/var/cache/huggingface"                 # optional; Hugging Face cache for the encoder and tokenizer
max_length = 2048                                    # optional; upstream's default is 2048
batch_size = 32                                      # optional; upstream's default is 32
```

The key names are upstream's verbatim (`docs/reference/toml_schema.md`
§`prefill_router` in the Switchyard repo), so its schema page transfers here
unchanged — the same reason the `type` discriminator exists at all (§3).
Targets are ordinary public model ids (ADR-0005 §2): each keeps its own
failover chain, pool, and adapter, and both the one-hop rule and the
unresolvable-target warning apply exactly as they do to every other type.
`router` and `upstream_model` stay mutually exclusive.

The **schema** is identical in both builds, feature on or off, and so are the
rules a feature-on build enforces: `targets` must be non-empty with no repeated
target (compared after the trailing `[1m]` hint is stripped, as everywhere
else), `checkpoint` must be non-empty, and `max_length` and `batch_size`, when
present, must be greater than zero. A blank target and a target that is itself
a router are rejected by the shared checks that already cover every type.

What differs is which of those a binary reports. Without the feature,
`validate_router` refuses a `prefill_router` entry by naming the cargo feature
*before* reaching any check above, so a release binary reports that one error
and none of the key-level ones (see "With the feature **off**" below).

### Why a cargo feature

Upstream's `crates/prefill-router` embeds Python through `pyo3` with
`auto-initialize`. Building it links libpython, and running it needs the
router's Python packages — `torch`, `transformers`, `numpy`, `accelerate`
(upstream's `crates/prefill-router/pyproject.toml`) — importable in the
interpreter the binary embedded, plus a router checkpoint on disk. That is not
a cost every shunt deployment can be made to pay, and upstream gates it the
same way; there is no other shape (ADR-0005 fact 5).

So `prefill-router` is a cargo feature, **off by default**. The release
workflow builds `--features ui` and never enables it, so no release binary and
no Homebrew install has the algorithm compiled in; a from-source build is the
only way to get it:

```sh
cargo build --release --features prefill-router          # add ,ui for the dashboard
```

With the feature **off** the config still parses — the schema is
unconditional — but the load fails, so `shunt check` reports it rather than a
process starting without the algorithm it was configured for. The feature
error is reported *ahead of* every other complaint about the table: an empty
target or a blank checkpoint would send the operator to fix a key that still
would not load, so those key-level rules are what a feature-on build reports.
Without the feature the entry reports only this:

```text
models entry <id> router type = "prefill_router" is not compiled into this binary: it needs the `prefill-router` cargo feature, which is off by default and absent from release binaries; build from source with `cargo build --features prefill-router` (docs/routing-algorithms.md)
```

### How it is hosted

`prefill_router` is an algorithm on ADR-0005 §1's **driven lane**: the
ordinary ladder lands the turn on the entry's default target provisionally,
`proxy::failover` admits the request against the entry's dependency envelope
(§5), and only then hands it to `libsy::drive`; the outcome's selected target
then replaces the provisional chain exactly as a judge verdict does.

- **Construction is per runtime state.** Each prefill entry builds upstream's
  `PrefillRouterAlgo` when the runtime state is constructed — at startup and
  on every hot reload. A failure there (a missing Python package, an
  unreadable or incompatible checkpoint) is a startup error, and on a reload it
  is a refused reload, so the running config stays up:

  ```text
  models entry <id> router type = "prefill_router" failed to load: <upstream error>
  ```

  `shunt check` only validates; it never loads a checkpoint.
- **A reload forgets the sessions.** Upstream's per-session affinity map lives
  inside the algorithm, so rebuilding it on reload drops which target each
  session had.
- **The request is translated down, not forwarded.** shunt turns the Anthropic
  body into upstream's request shape carrying only `user` and `assistant`
  roles and only `text` and `tool_result` blocks. That is all the algorithm
  reads: it scores the latest text user turn, and its affinity tells a human
  turn from a tool continuation by "every block in this message is a
  `tool_result`".
- **Session identity comes from the hint headers.**
  `x-claude-code-session-id`, plus `x-claude-code-agent-id` for a delegated
  child, so a tool continuation reuses the turn's decision instead of
  re-running inference. A caller that sends no session id falls back to
  upstream's own rule, a hash of the first user message.
- **Inference is off the async path.** It runs on a blocking worker, one
  prediction at a time per entry.
- **Admission comes first (issue #633).** The drive is inference over the
  caller's transcript and an affinity write keyed on the caller's session id,
  so it runs only after `[server.auth]` and the managed-model policy have
  admitted the request. Admission cannot wait for the decided chain — the
  chain is what the drive produces — so it ranges over the entry's
  dependency envelope instead: the caller must authenticate if *any* target
  injects a credential. `resolve_chain` parks the drive on
  `StageContext::drive_prefill` the way the stage router parks its judge
  consultation, and a turn a `[models.subagents]` overlay diverts before the
  router runs is not driven at all. A refused caller therefore triggers no
  inference and seeds no affinity for a session id it never proved it owns.
  Note what that does *not* say: an entry whose targets are all passthrough
  injects no credential anywhere in its envelope, so `check_inbound_auth`
  takes its `!injects_credential` path and refuses nobody — every caller,
  anonymous included, reaches the drive. The ordering closes the window in
  which a caller the gate *would* refuse got there first; it does not put a
  gate where the configuration asked for none.
- **`count_tokens` probes are driven too**, and on a continuation that is an
  affinity hit rather than an inference. Because the probe is a drive, it is
  admitted against the envelope like the turn it measures — not against its
  first route alone — and keeps first-route-only dispatch after the decision.

Three outcome labels appear on `x-gateway-route-source` and in
`shunt.router.decisions{algorithm="prefill_router"}`:

| Label | When |
| :-- | :-- |
| `prefill` | libsy drove the request — an inference or an affinity hit |
| `prefill_fail_open` | The drive errored; the request went to the **default target**, which is upstream's rule: the first target |
| `prefill_default` | A body-less resolution — `resolve_model`, discovery, `GET /routes` — has no turn to score, so it lands on the first target |

`GET /routes` lists the entry with `algorithm: "prefill_router"` and its
targets. The `shunt.stage_router.*` series never gain prefill rows: they are
the signal-only router's, and this algorithm is not it.

### Building it locally

The feature needs an interpreter, its packages, and a checkpoint.

**Point pyo3 at the right interpreter.** Its build script takes whatever
`python3` it finds first on `PATH`. On macOS with a pyenv shim in front that
was a Python 3.6 and the build failed outright. Set `PYO3_PYTHON` to the
interpreter whose environment actually has the packages — 3.10 or newer, with a
shared libpython:

```sh
uv venv && uv pip install torch transformers numpy accelerate
PYO3_PYTHON=.venv/bin/python cargo build --features prefill-router
```

**Bring your own checkpoint.** Switchyard v0.3.0 ships no checkpoint, no
exporter, and no encoder assets — upstream says so itself. The operator has to
obtain or train a checkpoint compatible with the router, and `checkpoint` must
name it; nothing in shunt produces one.

The two error strings above are the two ways this shows up: the feature-off
string means the binary was built without `--features prefill-router`, and the
`failed to load` string means the feature is on but the Python environment or
the checkpoint is not what the algorithm needs.

### What CI proves

`tests/prefill_router.rs` carries both halves.

- The **feature-off load error** runs in the default build step, which is the
  build every release binary is. That is the assertion that matters for
  operators: a config naming `prefill_router` against a binary without the
  feature fails to load with the message that names the feature.
- The **live test skips itself** unless `SHUNT_PREFILL_ROUTER_CHECKPOINT`
  names a checkpoint *and* `python3 -c "import torch, transformers"` succeeds.
  CI installs no Python packages, so the feature is verified there as a
  build-and-validate surface only — **CI does not exercise inference**, and a
  green run is not evidence that a checkpoint loads.

`benches/stage_router.rs` gains a `prefill_messages_from_body` arm behind the
feature (`cargo bench --features bench,prefill-router`); CodSpeed does not run
it.

### Not in this PR

Everything else on the driven lane. `advisor` and `llm_classifier`'s
`escalation` mode still make judge calls shunt does not host — naming either is
a load error — and they are PR 6 of the ADR-0005 §8 sequence.
`stage_router.classifier` has since shipped as PR 4 (§5), and `llm_classifier`
(capability and custom), `composite`, and the `[models.subagents]` classifier
form as PR 5 (§7). `prefill_router` is
the exception only because its inference is local: it needs no internal model
call. It does share PR 4's admission ordering — the envelope gate, then the
drive — since issue #633 closed the window in which it ran ahead of inbound
auth.

## 7. The driven lane: `llm_classifier`, `composite`, the stage classifier trigger, and the subagents classifier form (PR 5)

### What it is

PR 4 (§5) built the host contract — the dependency envelope, admission ahead of
`drive`, the internal `serve`, and the six per-call bounds — and spent it on one
algorithm, the stage router's `[models.router.classifier]` fallback. PR 5 spends
it on the rest of the judge-backed lane ADR-0005 §8 sequences here:
`type = "llm_classifier"` in its `capability` and `custom` modes,
`type = "composite"`, the `classify_trigger` key that decides *when* a judge is
consulted at all, and the `[models.subagents]` classifier form that routes a
delegated child.

Everything these add sits behind libsy's `Algorithm` trait. shunt builds the
algorithm at startup, hands it the request and the runtime model groups, serves
each `CallModel` it asks for, and feeds `selected_model_ids.first()` back into
the ordinary ladder — the same shape §5 already established. What is new is the
number of decisions libsy now makes for shunt, and therefore the number of
decisions shunt must *not* re-derive: the fallback, the retention, and the
session affinity are all upstream's, and this section is mostly an account of
where each of them lives.

| Module | What it holds |
| :-- | :-- |
| `src/config/router/classifier.rs` | `LlmClassifierConfig` and its two payloads, the shunt `ClassifyTrigger` enum |
| `src/config/router/composite.rs` | `CompositeRouterConfig` — the `classifier` and `stage` sub-tables |
| `src/config/subagents/classifier.rs` | The overlay's `mode = "custom"` payload |
| `src/config/router/validate.rs` | The load-time rules below, each with its own `ConfigError` variant |
| `src/routing/driven.rs` (with `driven/build.rs`, `driven/budget.rs`, `driven/drive.rs`) | `DrivenRouters`, the per-entry `Arc<dyn Algorithm>`, the judge budget, and `drive` |

### The three config forms

**`type = "llm_classifier"`, `mode = "capability"`.** A judge reads the
transcript and returns a solve probability; the entry routes to `weak_target`
when that probability clears `base_threshold` and to `strong_target` when it
does not.

```toml
[[models]]
id = "claude-judged"
[models.router]
type = "llm_classifier"
mode = "capability"                  # required here; upstream defaults it, shunt does not
classifier_target = "claude-haiku-4-5"   # consulted, never served
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
base_threshold = 0.5                 # required, as upstream
threshold_step = 0.0
classify_trigger = "every_request"   # or "user_turn", "new_session"
message_hash_fallback = false
max_output_tokens = 4096
# prompt = "…"                       # replaces the packaged capability prompt
```

**`type = "llm_classifier"`, `mode = "custom"`.** The judge returns JSON the
operator's own `response_schema` describes; a JSON Pointer picks a **group
name** out of it, and the first model of that group serves the turn. `any` and
`judge` are reserved and required; every other group is the operator's, which is
how one entry chooses between more than two models.

```toml
[models.router]
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
prompt = "Select exactly one target for this turn. …"
response_schema = '''
{"type": "object",
 "properties": {"target": {"type": "string", "enum": ["capable", "efficient"]}},
 "required": ["target"],
 "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }
```

**`type = "composite"`.** A judge sets the tier the stage router falls open to,
and leaves its scoring alone. The stage table takes **no `picker`** — the
classifier is the picker — so a stray `picker` key is an unknown-field error.

```toml
[models.router]
type = "composite"
[models.router.classifier]
target = "claude-haiku-4-5"
base_threshold = 0.5
classify_trigger = "user_turn"       # required; "every_request" is rejected
[models.router.stage]
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
confidence_threshold = 0.5
recent_turn_window = 3
capable_hold_turns = 0
```

**`[models.subagents]` with `type = "llm_classifier"`.** The overlay's second
form, and the one upstream documents
(`docs/routing_algorithms/subagent_routing.md` in the pinned checkout), written
here with shunt's public model ids:

```toml
[models.subagents]
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
classify_trigger = "new_session"     # the default here, and the only trigger accepted besides itself
max_output_tokens = 64
prompt = """
Select exactly one target for the delegated task.

- Select "capable" for code review, critique, auditing, or correctness analysis.
- Select "efficient" for implementation, research, explanation, and other delegated work.

Return only JSON matching the response schema.
"""
response_schema = '''
{"type": "object",
 "properties": {"target": {"type": "string", "enum": ["capable", "efficient"]}},
 "required": ["target"],
 "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }
```

Only `mode = "custom"` exists on the overlay, so `mode = "capability"` is an
unknown-variant error rather than a silent capability classifier on a child.
`classify_trigger` defaults to `new_session` (ADR-0005 §5) and `user_turn` is
rejected at validation: a child's turns are one delegated task, and re-judging
it mid-task would spend a judge call per tool step for a decision that cannot
change. `message_hash_fallback` must be `false` — the overlay is already keyed
on `(session, agent)`, and hashing the first message would key two different
children of one session onto one verdict.

**`mode` is required on both `llm_classifier` forms**, which is a deliberate
deviation from the upstream schema's `mode` default of `capability`. The three
modes route on entirely different principles, and the one that is not here yet —
`escalation` — is the one that buffers a served turn. An omitted `mode` reading
as `capability` would make adding `escalation` later a silent change of meaning
for a config that never named it. `mode = "escalation"` is therefore an
unknown-variant load error naming the two modes that do exist.

**`classify_trigger` on the stage router.** `[models.router.classifier]` (§5)
gains the same key, defaulting to `every_request`. `user_turn` consults the
judge only when the latest message is a human user turn — `role: user` carrying
at least one block that is not a `tool_result` — so a tool continuation rides
the pin instead of paying a second judge call. `new_session` behaves exactly as
`every_request` here; upstream says so of its own stage route ("`new_session`
has no effect here"), because the stage router already holds its decision in
shunt's own pin rather than in the algorithm. The key is decided in the
synchronous `routing::stage::select` pass — the same place the consultation
itself is set (§5) — and so before any judge is reached.

**Bounds.** The six per-call keys §5 introduced live on `[models.router]` of
every driven type and on a classifier-form `[models.subagents]` — every table
that makes internal calls carries its own copy. A `0` in any of them is a
startup error naming the key, as before.

### Why the algorithm's own fallback is the policy

libsy's judge folds **every** call failure into "no verdict": a `400`, a
timeout, an unparseable reply, and an empty one all arrive at the same place.
The policy then falls back to the configured default category — `strong_target`
in capability mode (libsy's capability default is `Capable`), the first model of
the `default_target` group in custom mode, `stage.efficient_target` on a
composite — and stamps evidence `{"source": "fail_open"}` on the outcome.

shunt does not second-guess that. A driven entry's failure path is *one* judge
call and then the algorithm's own default: libsy issues exactly one `CallModel`
carrying the whole judge list, and shunt's `serve` dispatches to
`call.models.first()` only, so a `400` from the first judge candidate means
**zero further judge calls** — not a walk down `models.judge`. The turn is
answered, the route source is `classifier_fail_open`, and the client still gets
a `200`.

That is the concrete shape the live captures predicted. A judge target whose
schema the Anthropic grammar refuses answers
`400 {"type":"error","error":{"type":"invalid_request_error","message":"output_config.format.schema: For 'number' type, properties maximum, minimum are not supported"}}`
(`docs/notes/adr-0005-routing-live-captures.md`, "Fact (c), re-captured") — the
first call, no verdict, nothing to parse. It is also why the pinned codec's
recursive strip of `minimum`, `maximum`, `minLength`, and `maxLength` from the
schema is load-bearing rather than cosmetic.

The successful shape from the same capture is what the integration tests are
built on: `output_config.format = {"type": "json_schema", "schema": …}` —
`schema`, not `json_schema` — a **string** `system`, `max_tokens: 4096`, and no
`tools`, `tool_choice`, or `stream`. A real reply carried `stop_reason:
end_turn` and a `content` array of exactly one `text` block holding the verdict
JSON, e.g. `{"crux": "…", "primary_rule": "SUP-1", "capability_boundary":
"supported", "p_solve": 0.82}`. Read the capture's scope narrowly: six replies
on `claude-haiku-4-5-20251001` validated against the packaged schemas with no
`json_object` fallback; the ten other Claude ids in that run never evaluated the
field at all and are **untested, not failing**.

ADR-0005 §3's `fail_open` — "a judge or review bound failure *after a complete
retained turn* resolves as the algorithm's `fail_open` outcome" — is a different
clause about a different lane. No turn is retained here, because retention is
`escalation` and `advisor`, which are PR 6. What this PR has is the no-verdict
default above.

### Sessions live inside the algorithm

Affinity — which target a session or a delegated child is holding — is
upstream's state, and it lives in the algorithm instance
(`AffinityRouter.assignments`). So the constructed `Arc<dyn Algorithm>` must
outlive the request: `DrivenRouters` is built once per `RuntimeState`, keyed by
model id, immediately after `PrefillRouters` and threaded onto `AppState` the
same way. Two consequences follow from that and are worth stating rather than
discovering:

- **A reload forgets the sessions.** Rebuilding the runtime state rebuilds the
  algorithms, so `user_turn` retention and `new_session` affinity start over.
  This is the same property `prefill_router` has (§6) and for the same reason.
- **A construction failure is a startup error.** Validation builds the
  `LlmTaskClassifier`/`CompositeRouter` once and drops it, so a config libsy
  refuses — a prompt containing `{{RESPONSE_SCHEMA}}`, or a standalone
  classifier's `message_hash_fallback` without `new_session` — is reported by
  `shunt check` rather than at the first request. shunt re-implements those
  rules as its own `ConfigError` variants so the message names the shunt key,
  and the construction is the backstop. The pairing rule above is the
  standalone form's alone: a composite owns the tier retention itself, refuses
  only `every_request`, and accepts the hash key under either of the triggers
  its table can spell.

`max_judge_calls` is shunt's, not libsy's, and is counted in a `JudgeBudget`
keyed on `sha256(session_id ‖ agent_id)` — so a `Task` child spends its own
budget rather than its parent's, the same scoping the stage pin key took in
PR 1 (§2). The map is capped at 4096 entries. A request carrying no session id
is not tracked at all: there is no key to accumulate under, so the bound applies
per request for those callers. A turn that finds its budget spent skips the
drive entirely, answers from the algorithm's fail-open target, and records the
judge-call outcome `budget_exhausted`. The reservation is taken per `CallModel`
rather than per drive, so the same label also covers a call refused *inside* a
drive — two turns of one session racing for the last one, or a chaining
algorithm (composite, subagents classifier) asking for a second call on a
budget down to one. Those turns answer from the algorithm's own close, not from
a skipped drive, but the outcome they record is still `budget_exhausted`.

### Probes

`count_tokens` is `read_only` and never enters `drive`, exactly as on the stage
router (§5). A probe on a driven entry resolves to the entry's `fail_open`
target — the same default a no-verdict turn takes — with **zero judge calls**,
and is gated and dispatched against that target's first route only. The same
holds for a delegated probe against a classifier-form overlay: the overlay
answers from its `default_target` group's first model, and the child is not
classified by a request that is not a turn.

The body-less surfaces behave the same way for the same reason: `GET /routes`,
`/v1/models` discovery, and `shunt check` have no transcript to judge, so a
driven entry reports its fail-open target there, under route source
`classifier_default`.

### Responses judge targets

A judge target can map an OpenAI Responses provider, and the Anthropic ⇄ neutral
codec at the routing boundary emits `output_config.format` (above), which the
Responses wire shape does not have. `src/model/responses_request.rs` — shunt's
own translator, not the crate's (§5) — therefore maps it: a request carrying
`output_config.format` with `type == "json_schema"` and a `schema` object
becomes `text.format = {"type": "json_schema", "name": "verdict", "schema": …,
"strict": false}`, merged into the existing `text` object. The Xai/Grok flavor
is excluded, as it is for the other `text` keys that path already writes.

Two translators, two jobs, still: the neutral ⇄ Anthropic direction is
`switchyard-translation`'s `anthropic_messages` codec, and Anthropic → Responses
stays shunt's.

### Observability

`x-gateway-route-source` and the `source` label on
`shunt.router.decisions{algorithm}` gain four values, and the mapping from
libsy's own evidence to them is a closed table read off the pinned revision
rather than a pass-through of arbitrary strings:

| Evidence `source` | Route source | Label |
| :-- | :-- | :-- |
| *(absent — a body-less or unconsulted resolution)* | `RouteSource::DrivenDefault` | `classifier_default` |
| `fail_open` | `RouteSource::DrivenFailOpen` | `classifier_fail_open` |
| `fall_open` on a classifier entry (`DefaultCategoryClassifier` closing the cascade) | `RouteSource::DrivenFailOpen` | `classifier_fail_open` |
| `retained` | `RouteSource::DrivenRetained` | `classifier_retained` |
| `affinity`, or anything that is not a `DecisionSource` string | `RouteSource::Driven` | `llm-classifier` |
| a `DecisionSource` string (composite's stage route) | `RouteSource::Stage(tier, Scorer(source))` | the stage router's own labels |

Upstream spells the two fallbacks one letter apart and they are not the same
path: `fail_open` is the judge call itself failing (`util/llm_judge.rs`), while
`fall_open` is `DefaultCategoryClassifier` closing the cascade
(`llm_class.rs`) — *and* is libsy's stage picker default, which is why the
reading depends on whether the entry is a composite one. On a classifier entry
both mean "no verdict decided this turn"; on a composite entry `fall_open` is
the scorer's own default and stays a stage row.

`algorithm` is `llm_classifier`, `composite`, or — for the overlay —
`subagents`, matching what `GET /routes` reports. The composite row is the
reason the table is closed rather than a string copy: libsy's stage route inside
a composite writes a `DecisionSource`, which shunt already spells as
`StageSource::Scorer` (§1), and re-spelling it as an opaque classifier source
would make a composite's signal-driven turns unreadable against a plain stage
router's.

`shunt.router.judge_calls{model, algorithm, outcome}` keeps the closed outcome
set PR 4 defined — `decided`, `timeout`, `oversized`, `upstream_error`,
`invalid_reply`, `translation_failed`, `budget_exhausted` — and the driven lane
records `retained` for a turn answered from affinity with no call made.
`GET /routes` lists a driven entry's `targets` (the answer destinations: both
tiers for capability and composite; for custom, every non-`judge` group's
models, groups in name order and each group's ids in the order it declares
them) and its `judges` (`classifier_target`, `classifier.target`,
or every member of `models.judge`) — consulted, never served. Verdict text is
never logged: the `tracing` record on a drive carries labels only.

### What is tested

Unit, alongside the config: one validation module and one round-trip module per
form (capability, custom, composite, the stage classifier's `classify_trigger`,
the subagents classifier), with the round-trip TOML taken verbatim from ADR-0005
§2 or upstream's own example where one exists;
`the_one_hop_rule_covers_every_router_type` gains the two new types; and
`/routes` reports `targets` and `judges` for a custom entry. In
`src/routing/driven/tests.rs`: the evidence→source table above, an outcome
naming a target outside the entry's `targets` falling open, and the budget's
key scoping and bound.

Integration, in `tests/driven_lane.rs` (with the overlay, probe, and budget
clauses in `tests/driven_lane/overlay.rs`, split only for the file-length
ceiling), one focused test per clause:

1. a capability verdict in the **verbatim captured Anthropic reply shape** (one
   `text` block, `stop_reason: end_turn`) routes the turn — `p_solve 0.82` to
   the weak tier under `base_threshold = 0.5`, `0.10` to the strong one — and
   the captured judge **request** is asserted field by field: `output_config
   .format.type == "json_schema"`, no `tools`, no `tool_choice`, a string
   `system`, `max_tokens == 4096`, no `stream` key, and no `minimum`/`maximum`
   anywhere in the schema;
2. a custom verdict from an **OpenAI `json_schema` reply** — a Responses SSE
   stream — routes the turn, with the translated upstream request carrying
   `text.format.type == "json_schema"`;
3. a `400` from the judge is a classifier failure: the turn lands on
   `default_target`'s first model under `classifier_fail_open`, the client still
   gets `200`, and a second configured judge candidate is called **zero** times;
4. a composite `user_turn` consults the judge once and the tool continuation
   after it does not;
5. a delegated turn is classified once per `(session, agent)`: the parent makes
   no judge call, a second turn of the same child makes none, a different agent
   id makes one more, and an unauthenticated delegated turn is refused with zero
   — the envelope covers the overlay's judge;
6. a stage classifier with `classify_trigger = "user_turn"` skips a
   tool continuation, which resolves as `fall_open` with no judge call;
7. a `count_tokens` probe on a classifier entry makes no judge call;
8. `max_judge_calls = 1` under `every_request` judges the first turn and answers
   the second from the budget-exhausted fail-open path.

### Not in this PR

`mode = "escalation"` and `type = "advisor"` — the two algorithms that serve the
answer while routing — have since shipped as PR 6, §8 below, with the
buffer-and-replay lane the three `gated_*` bounds were built for and the
`AGENTS.md` streaming amendment ADR-0005 §4 asks for. `mode` stays required, so
`escalation` arrived as a new value rather than as a change of meaning for a
config that omitted the key.

An entry carrying no `[models.router]`, or one whose `type` is on the pure lane,
is untouched: no algorithm is constructed for it, no envelope is walked, and it
pays the same one `Option` check it paid before.

## 8. The buffer-and-replay lane: `escalation` and `advisor` (PR 6)

### What it is

The two algorithms that **serve the answer while routing**: `type =
"llm_classifier"` with `mode = "escalation"`, and `type = "advisor"`. Both make
the client's turn first, hold it, have a judge or a reviewer rule on the
*completed* turn, and only then decide whether that turn is what the client
gets. That makes them the one place shunt buffers an upstream response the
client asked to stream, which is the `AGENTS.md` amendment ADR-0005 §4 asks for
and this PR makes:

> Preserve streaming semantics; do not buffer upstream SSE responses unless the
> client requested non-streaming output or the entry's router (`escalation`,
> `advisor`) requires the completed turn before serving it.

The scope is the point of the wording. Only a **gated** turn is buffered, and
only on an entry that selects one of these two algorithms. Every other route,
and every turn on these routes that is not gated, streams exactly as before.

```toml
[[models]]
id = "claude-escalate"
[models.router]
type = "llm_classifier"
mode = "escalation"                    # the third mode; `mode` is still required
classifier_target = "claude-haiku-4-5" # the judge — consulted, never served
strong_target = "claude-opus-4-8"      # served once the session latches
weak_target = "claude-sonnet-4-6"      # served before the latch
# prompt = "…"                         # replaces the packaged trajectory-judge prompt
# max_output_tokens = 4096
[models.router.escalation]             # optional; shown at its defaults
confirmations = 2                      # consecutive escalate verdicts to latch; above 1 needs a session id
recent_turn_window = 28
window_message_chars = 500             # at least 50

[[models]]
id = "claude-reviewed"
[models.router]
type = "advisor"
executor_target = "claude-sonnet-4-6"  # serves every client-visible turn
advisor_target = "claude-opus-4-8"     # reviews gated turns — never served
gate_trigger = "no_tool_call"          # or "pattern", with gate_trigger_pattern
max_reviews = 1
# gate_stall_turns = 0                 # 0 = off
# gate_min_tool_results = 0
# advisor_max_tokens = 2048
# advisor_temperature unset = omitted from the review request
# transcript_max_chars = 200000
# fail_open = true
```

Key names, defaults, and ranges are upstream's
(`docs/reference/toml_schema.md` in the pinned checkout), as ADR-0005 §2 fixes
for every type. Both entries also carry the six per-call bounds (§5). The
`[models.subagents]` classifier overlay stays `mode = "custom"` only; neither
new form is accepted there.

Both are driven-lane entries in every respect §5 and §7 established. Each is
admitted against its dependency envelope before any call. A judge or advisor
target that resolves to a passthrough route is a startup error, for the §5
reason: an internal call strips every caller credential slot, so a passthrough
route has nothing to run on. The **weak and executor targets may be
passthrough**, because the gated turn is not an internal call. It is the answer
dispatch of the selected target, so it establishes its own primary origin from
that target's chain and carries the caller's credential exactly as a live turn
does (ADR-0005 §3). A `count_tokens` probe never enters `drive`: it makes zero
judge calls and zero gated calls, and answers from the weak or executor target.

The advisor's `max_reviews` budget and its failure cap are per session. The
pinned `AdvisorGate` folds every request that carries no session id into one
instance-wide scope that lasts until the config reloads, so one anonymous
caller would spend review for every other. shunt instead gives a sessionless
advisor turn a request-unique session id marked final, which libsy drops when
the drive returns: each sessionless request is reviewed as a session of its
own (`sessionless_advisor_turns_do_not_share_one_review_budget`). The judge
budget's key is read before that id is set, so `max_judge_calls` keeps its
per-request allowance for sessionless callers.

### Which turns are gated

| Entry | Gated (buffered) | Live (streamed as before) |
| :-- | :-- | :-- |
| `escalation` | The weak turn, on a session that has not latched | Every turn after the latch, served by `strong_target`; the strong call that replaces a discarded weak turn |
| `advisor` | Every executor turn while the session still has review budget and fewer than three failed advisor consults | Every turn once `max_reviews` is spent or the advisor has failed three consults (a failed consult refunds its review); the executor's re-run after a REDO |

The advisor gates every executor turn in a session that has budget left and
has not hit the failed-consult cap, not only the turns that end up reviewed.
The algorithm cannot know whether a turn trips `gate_trigger` until the turn is
complete — `no_tool_call` is a property of how the turn ends — so it holds each
one and releases the ones that do not trip it without a review. Once the budget is spent, libsy routes to the
executor with no gate at all, and shunt streams that turn live.

### Capture, terminality, and replay

A gated turn is made **in the caller's own mode**:

- **`stream: true`.** The call streams, and the SSE frames the adapter renders
  are retained as they arrive. The judge's neutral `Response` is assembled from
  them. If the turn is served, the retained frames are replayed byte for byte.
  They are retained, not re-encoded.
- **`stream: false`.** The call is non-streaming. The single JSON message is
  retained, the judge's `Response` derives from it, and the client gets that
  message.

No SSE-to-JSON conversion exists or is added: the gate changes *when* the answer
is sent, never its shape.

**Replayed under the advertised id.** The internal dispatch carries the
router's own id as the alias to render, so `message_start.model` — or the
`model` of the JSON message — is written as the id the client asked for
*before* anything is captured. A frame captured under the executor's id and
replayed verbatim would hand Claude Code the wrong model to restore on
`--resume` (issue #172). "Byte-faithful" therefore means faithful to what the
client would have received live, and it holds for an Anthropic executor and an
OpenAI Responses executor alike.

**Headers are committed only when the replay starts.** Until then the client
has received nothing, so a discarded turn — a REDO, a latch, a cut weak turn —
crosses no post-2xx boundary. ADR-0002's post-2xx no-replay rule is not bent:
the client never saw the turn that was dropped.

**A retained turn is servable only after an authoritative terminal marker.**
On a streaming call that marker is `message_stop`. On a non-streaming call it is
a complete body that parses as one message. A turn without its marker is never
served, whatever the upstream status was. A truncated `200` is never replayed.

**Gated turns take the ordered failover chain.** A pre-header failure on the
gated call advances down the target's chain like any other dispatch. A
streaming turn on a multi-route chain with an HTTP Responses route takes the
committed chain stream the live path uses for that shape: the Responses
adapter commits a synthetic `200` before it sends, so the ordered loop would
see its failure only as an in-stream `error` frame and read a cut turn
(`a_streaming_gated_turn_fails_over_along_the_weak_chain`). A non-2xx answer
that ends the chain is relayed to the client unchanged, as a live turn's would
be.

### Why the gated call is the first `CallModel`

Both pinned algorithms make the gated call before any judge or review call:
escalation calls the weak target and buffers it before it builds the judge
request, and the advisor calls the executor and buffers it before the gate
decides whether to consult. So shunt identifies the gated call as the **first
`CallModel` of a drive**, not by comparing the call's target to a configured id.
Two consequences follow:

- **The gated call is not charged to `max_judge_calls`.** That bound counts
  judge and review calls only (§7). A turn costs one gated call plus at most the
  judge or review calls the budget allows.
- **`weak_target` may equal `classifier_target`.** A target-id comparison could
  not tell a weak turn from a judge call to the same model. Position can, so
  shunt does not need upstream's prompted-target restriction.

Every later `CallModel` of the same drive is an internal call and goes through
§5's `serve` with the caller's credentials stripped.

### Bounds

The three `gated_*` keys, validated since PR 4 but inert until now, bound every
gated turn:

| Key | Default | Bounds |
| :-- | :-- | :-- |
| `gated_max_bytes` | `8388608` | Retained SSE frame bytes, or the retained JSON body |
| `gated_idle_ms` | `60000` | The gap between body chunks. On a streaming call only a completed content frame counts as progress (§5), so SSE `ping` frames do not reset it and an endless keep-alive stream cannot hold a gated turn open |
| `gated_max_duration_ms` | `600000` | Wall clock across headers and body |

Crossing a bound cancels the upstream call and refunds nothing: the upstream
already spent the tokens. The `judge_*` keys and `max_judge_calls` keep their
§5 meaning for the judge and the review.

### Route sources

`x-gateway-route-source` and the `source` label on
`shunt.router.decisions{algorithm}` gain a closed set of values. Every replayed
turn carries a label that names it as one, so a client can tell a replayed turn
from a live one.

| Label | Entry | What happened | Delivery |
| :-- | :-- | :-- | :-- |
| `escalation_weak` | escalation | The judge let the weak turn through: it declined, or the escalate streak is still below `confirmations` | Replayed |
| `escalation_latch` | escalation | The session latched, on this turn or an earlier one, so the strong target served the turn | Live |
| `escalation_fallback` | escalation | The weak turn failed or was cut before its terminal marker, so the strong target served the turn. The judge was not called | Live |
| `classifier_fail_open` | escalation | The judge failed after a complete weak turn, so the weak turn was served | Replayed |
| `advisor_approve` | advisor | The executor turn was reviewed and APPROVEd | Replayed |
| `advisor_pass` | advisor | The executor turn was served without a review: it did not trip the gate (for example, it ends in a tool call), or no review could be reserved | Replayed |
| `advisor_fail_open` | advisor | The review failed after a complete executor turn, so the turn was served | Replayed |
| `advisor_redo` | advisor | The reviewer said REDO. The discarded turn was never sent; this is the executor's re-run | Live |
| `advisor_exhausted` | advisor | The session's `max_reviews` is spent, or its advisor has failed three consults (a failed consult refunds `max_reviews`), so the executor streams with no buffering | Live |
| `gated_error` | either | The gated turn could not be served (see the failure matrix below) | Error |

Unlike the classifier labels in §7, these are shunt's names for libsy's
evidence, not copies of it. The mapping reads the `source` and `verdict` keys
libsy sets on the outcome at the pinned revision (`algorithms/escalation.rs`,
`algorithms/advisor_gate.rs`), plus whether the outcome carries a retained
response:

| libsy evidence | Outcome carries | Label |
| :-- | :-- | :-- |
| `{"source": "escalation", "verdict": "latched"}` | no response | `escalation_latch` |
| `{"source": "escalation", "verdict": "escalate"}` (streak confirmed on this turn) | no response | `escalation_latch` |
| `{"source": "escalation", "verdict": "continue"}` (the judge declined) | the weak turn | `escalation_weak` |
| `{"source": "escalation", "verdict": "pending"}` (escalate, streak below `confirmations`) | the weak turn | `escalation_weak` |
| `{"source": "fail_open", "reason_code": …}` from the judge (no verdict) | the weak turn | `classifier_fail_open` |
| `{"source": "fallback", "reason_code": …}`, or a weak turn shunt cut before its marker | no response | `escalation_fallback` |
| `{"source": "advisor", "verdict": "approve"}` | the executor turn | `advisor_approve` |
| `{"source": "advisor", "verdict": "fail_open"}` | the executor turn | `advisor_fail_open` |
| `{"source": "advisor", "verdict": "redo"}` | no response; a rewritten request | `advisor_redo` |
| no evidence | the executor turn | `advisor_pass` |
| no evidence, and no gated call was made | no response | `advisor_exhausted` |

`gated_error` has no evidence row: it is shunt's own outcome, set where the
host fails the request.

`shunt.router.judge_calls{algorithm}` is `llm_classifier` for an escalation
entry and `advisor` for an advisor entry, with §5's closed outcome set.
`shunt.router.decisions{algorithm}` uses the same two values. `GET /routes`
lists `classifier_target` or `advisor_target` under `judges`: consulted, never
served.

### Failure matrix

| What fails | `escalation` | `advisor` |
| :-- | :-- | :-- |
| The gated turn crosses a `gated_*` bound | Discarded before any header is committed; the strong target serves the turn live (`escalation_fallback`) | Discarded before any header is committed; the request fails with a gateway-owned `502` in the Anthropic error shape (`gated_error`) |
| The gated turn ends before its terminal marker | As above | As above. Not a REDO and not a failover attempt: the upstream answered `2xx` and the client never saw it |
| The gated call's upstream answers non-2xx | Relayed to the client unchanged, as a live turn would be (`gated_error`) | Relayed unchanged (`gated_error`) |
| The judge or review fails after a complete turn — timeout, oversized, unparseable, upstream error, or `max_judge_calls` spent | The algorithm's `fail_open` outcome: the weak turn is replayed (`classifier_fail_open`) | With `fail_open = true` (the default), the executor turn is replayed (`advisor_fail_open`). With `fail_open = false`, the request fails with a gateway-owned `502` (`gated_error`) |

The escalation column and the advisor column differ on a cut turn by design.
Escalation has a second answer on hand, the strong target, and taking it costs
nothing the client can see. The advisor has only the executor. Re-running the
executor on a turn that upstream already answered `2xx` would be a failover
retry after a successful response, and ADR-0002 rules that out.

### What it costs

Operators opt into these costs per entry, so the reference page states them:

- **Time to first token becomes time to last token on gated turns.** The client
  receives nothing until the turn is complete and judged.
- **Escalation spends a judge call on every un-latched turn.** A turn that
  latches also pays for a weak call it discards.
- **A discarded weak or executor turn still consumed its upstream quota.** It
  is counted as `caller = "client"` in `shunt.requests`, because it was the
  client's answer dispatch, not an internal call.

### What is tested

The ADR-0005 §8 definition of done for PR 6, one focused integration test per
clause, in `tests/buffer_replay.rs` and `tests/buffer_replay/advisor.rs`:

1. a replayed turn's `message_start.model` equals the router id on an
   Anthropic executor and on a Responses executor —
   `a_declined_streaming_escalation_turn_is_replayed_on_both_adapters`;
2. a `stream: false` caller receives one JSON message on a gated turn, on both
   adapters — `a_non_streaming_caller_gets_a_single_json_message_on_both_adapters`
   (the Responses upstream is always called streaming, so only the Anthropic
   run also asserts a non-streaming upstream request);
3. a REDO commits no headers before the re-run —
   `an_advisor_redo_serves_only_the_second_executor_turn`;
4. a judge or review failure after a complete retained turn resolves as
   `fail_open` — `a_stalled_escalation_judge_replays_the_weak_turn_it_retained`
   and `a_stalled_advisor_review_replays_the_executor_turn`;
5. a weak turn cut before `message_stop` is never replayed, and the strong call
   is made instead — `a_weak_turn_cut_before_message_stop_falls_back_to_the_strong_tier`;
6. a nonterminal advisor executor turn fails with a gateway-owned `502` before
   any header is committed — `a_nonterminal_executor_turn_is_a_gateway_error`.

Unit, alongside the code: both config tables' round-trip, defaults, stray-key,
and upstream-constructor rejections (`src/config/router/advisor/tests.rs`,
`src/config/router/classifier/tests.rs`); the evidence → source table above
(`src/routing/driven/gated/tests.rs`); and the terminal-marker scan and the
single-message check (`src/routing/serve/gated/tests.rs`).

### Not in this PR

`prefill_router` is PR 7, shipped as §6 above. With this PR every algorithm in
ADR-0005's list is hosted.
