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
below; the rest of the sequence is not implemented yet.

## 2. Request hints, the pin scope, and the compaction latch (PR 1)

### What it is

`src/routing/context.rs` reads five inbound headers into a `RouterContext`,
built once per request and stored nowhere:

| Header | Read as |
| :-- | :-- |
| `x-claude-code-session-id` | The session the pin is keyed on |
| `x-claude-code-agent-id` | The delegated agent the pin is scoped to |
| `x-claude-code-request-class` | `main`, `subagent`, `workflow`, `compaction`, or `auxiliary`; an unrecognised value reads as absent |
| `x-claude-code-agent-type` | Carried for the `[models.subagents]` `by_type` map (ADR-0005 §8 PR 3); nothing routes on it yet |
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

The `[models.subagents]` `by_type` map and the read-only carve-out ADR-0005 §11
describes for the `compaction` and `auxiliary` classes are later steps in the §8
sequence. `x-claude-code-agent-type` is carried but routed on by nothing, and
`x-claude-code-compaction` and `x-claude-code-prev-tool-durations` are read by
nothing and are therefore deliberately absent from `/protocol`'s consumed list.

A request carrying none of these hints — a bare `curl`, a Claude Code older than
the headers, or the default gate-off deployment with no agent id — routes
exactly as it did before.
