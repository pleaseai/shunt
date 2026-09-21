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
below, PR 2 is §3, PR 3 is §4, PR 4 is §5, and PR 7 is §6; the rest of the
sequence is not implemented yet.

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
that describe judge traffic (ADR-0005 §7). `llm_classifier`, `composite`, and
`advisor` are PR 5 and are still load errors. `prefill_router` is PR 7, behind
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
runtime, and the collector is exercised by its own unit tests only.

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

The rest of the driven lane: `llm_classifier`, `composite`, and `advisor` are
PR 5, and naming one is still a load error. Buffer-and-replay — the escalation
weak turn and the advisor executor turn the `gated_*` bounds are built for —
is PR 6, and the streaming-semantics amendment `AGENTS.md` needs lands with it.
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

`prefill_router` is an algorithm on ADR-0005 §1's **driven lane**:
`proxy::failover` hands the request to `libsy::drive` before the ordinary
ladder runs, and the outcome's selected target re-enters that ladder exactly
as a stage tier does.

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
- **`count_tokens` probes are driven too**, and on a continuation that is an
  affinity hit rather than an inference.

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

Everything else on the driven lane. `llm_classifier` (all modes), `composite`,
`advisor`, and the `[models.subagents]` classifier form all make judge calls,
and remain PR 5 of the ADR-0005 §8 sequence — naming one is still a load error.
`stage_router.classifier` has since shipped as PR 4 (§5). `prefill_router` is
the exception only because its inference is local: it needs no internal model
call, no dependency envelope, and no admission rework.
