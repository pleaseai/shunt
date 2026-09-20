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
below and PR 2 is §3; the rest of the sequence is not implemented yet.

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

The driven lane is untouched: `llm_classifier`, `composite`, `advisor`, and
`stage_router.classifier` all make judge calls and are PR 4 onward, so a `type`
naming one of them is a load error rather than a silent pass-through.
`prefill_router` is PR 7, behind its own cargo feature. `[models.subagents]` is
PR 3 (§4). The `shunt.router.judge_calls` metric and the `judges` list on `GET
/routes` describe judge traffic and land with it (ADR-0005 §7).

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
| Body-less surfaces | `GET /routes`, discovery, and `shunt check` have no headers and report the parent's destination. The overlay is not listed in `/routes` `routers[]` |
| One new benchmark arm | `resolve_chain_subagents_passthrough`: the child's turn from `resolve_chain_routed_delegated`, once the routed entry also carries an overlay. Read against that arm — the gap is the scoring and the store read the child stops paying — and flat across turn counts |

### Not in this PR

The `llm_classifier` form of the overlay is PR 5 with the rest of the driven
lane. The read-only carve-out for `compaction` and `auxiliary` on the stage
router — landing them on the session's tier without recording — is still a
later step; absent the overlay they route as they do today, and with it they
route as the parent does. `GET /routes` does not yet describe the overlay.
