# ADR-0005 §10: the three live captures

External-behaviour note for the two endpoints ADR-0005's driven lane depends
on — the Claude Code client and `api.anthropic.com`. §10 ("Verification before
code") names three facts that must be observed on the wire before the
predicates keying on them are written, because a mock reproduces the *fact* of
a header or a reply but not its *shape*. This note records what a real
`shunt run` saw. It does not amend the ADR; where the wire disagrees with §11,
the disagreement is stated here and §11 is left as written.

## The rig

Every request below crossed a real gateway. Nothing is mocked and no fixture
was replayed.

```
Claude Code 2.1.276  ──http──▶  shunt run (127.0.0.1:31801)
                                  │  headers::filtered() forwards every
                                  │  non-hop-by-hop header verbatim
                                  ▼
                                recording tap (127.0.0.1:31802)
                                  │  logs method/path/headers/body, then
                                  ▼  forwards unmodified over TLS
                                a hosted shunt 0.44.0 deployment ──▶ api.anthropic.com
                                (pooled Anthropic subscription credentials)
```

The second shunt hop is only how this machine reaches Anthropic. The captured
upstream replies carried Anthropic request ids, so the origin of each `200`,
`400`, or `429` was never in doubt.

* **shunt**: `0.45.1`, debug build of `b4fa5fd1`, started as
  `shunt run --config …` with `default_provider = "anthropic"` and the
  `anthropic` provider's `base_url` pointed at the tap. `auth = "passthrough"`,
  so the client's own credential travels and shunt injects nothing.
* **Client**: `claude` `2.1.276`, driven two ways — headless
  (`user-agent: claude-cli/2.1.276 (external, sdk-cli)`) and an interactive
  REPL in a pty (`… (external, cli)`). Both are here because §11's `main` is
  "the REPL main thread or the SDK", and driving only the headless path would
  leave the primary case unverified.
* **Isolation**: every run after the first used a throwaway `HOME`, so the only
  agents in scope were the built-ins plus one deliberately custom project
  agent. This matters — the first attempt at fact (b) reported `custom` for
  `Explore`, because the real `HOME` has a `~/.claude/agents/Explore.md` that
  shadows the built-in of that name.
* **Model**: `claude-haiku-4-5-20251001`, the only Claude id that completed
  either forced tool-use request during the capture window (see "What this
  capture does not establish").
* Date: 2026-09-18. Credentials, account names, and org/workspace ids are
  redacted; nothing else in the quoted header blocks is altered.

## Fact (a) — `x-claude-code-agent-id` marks a delegated turn, and only one

Confirmed, on both entrypoints and in both gate states. Across **77**
`POST /v1/messages` requests from Claude Code, the header's presence is
exactly coextensive with delegated work: no `main` or `auxiliary` turn ever
carried it, and no `subagent` turn ever omitted it.

| `user-agent` | `request-class` | `agent-id` | `agent-type` | n |
|---|---|---|---|---|
| `… (external, cli)` | `main` | absent | absent | 3 |
| `… (external, cli)` | `auxiliary` | absent | absent | 1 |
| `… (external, cli)` | `subagent` | **present** | `Explore` | 3 |
| `… (external, cli)` | *absent (gate off)* | absent | absent | 4 |
| `… (external, cli)` | *absent (gate off)* | **present** | absent | 3 |
| `… (external, sdk-cli)` | `main` | absent | absent | 30 |
| `… (external, sdk-cli)` | `auxiliary` | absent | absent | 11 |
| `… (external, sdk-cli)` | `subagent` | **present** | see fact (b) | 15 |
| `… (external, sdk-cli)` | *absent (gate off)* | absent | absent | 5 |
| `… (external, sdk-cli)` | *absent (gate off)* | **present** | absent | 2 |

Two of the four requests whose agent type was `custom` predate the throwaway
`HOME` and came from the shadowed `~/.claude/agents/Explore.md` — which is a
custom agent, so `custom` is the right value for them. The other two are the
project's `marker-finder` (fact (b)).

A parent turn that went on to spawn an `Explore` child, verbatim — this one
from the headless driver; the `host` line is shunt's rewrite to the tap, not
the client's:

```http
POST /v1/messages?beta=true HTTP/1.1
accept: application/json
authorization: <redacted>
content-type: application/json
user-agent: claude-cli/2.1.276 (external, sdk-cli)
x-claude-code-session-id: 0f0a2cc3-d5f1-4200-b9c8-f56a081194ce
x-stainless-arch: x64
x-stainless-lang: js
x-stainless-os: MacOS
x-stainless-package-version: 0.112.1
x-stainless-retry-count: 0
x-stainless-runtime: node
x-stainless-runtime-version: v26.3.0
x-stainless-timeout: 600
anthropic-beta: interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11
anthropic-dangerous-direct-browser-access: true
anthropic-version: 2023-06-01
traceparent: 00-c2fb5f125ec74a4712d9ec13d0a341b8-085c71cc2fb063ae-01
x-app: cli
x-claude-code-request-class: main
accept-encoding: gzip, deflate, br, zstd
host: 127.0.0.1:31802
content-length: 85911
```

Its `Task` child, same session, three headers different:

```http
POST /v1/messages?beta=true HTTP/1.1
…
x-claude-code-session-id: 0f0a2cc3-d5f1-4200-b9c8-f56a081194ce
…
x-claude-code-agent-id: a7a11c2e22e29e67a
x-claude-code-agent-type: Explore
x-claude-code-request-class: subagent
…
content-length: 33828
```

Two properties of the id worth recording for the §5 store key. It is stable
across every turn of one child and distinct per spawn — ten children produced
ten ids (`a7a11c2e22e29e67a`, `acf659811cf929e90`, `a6544583269e5f238`,
`a8389ba99204847ca`, `a8c35f5ed2a07b83f`, `adaddc8ba7766f4d0`, …) — so hashing
it into the key partitions children from each other and from the parent, as §5
intends. And it is opaque: nothing in it identifies the agent type.

**One thing §11's table does not say, and shunt deployments depend on.**
`x-claude-code-agent-id` is **not** one of the five gated headers. A control
run with `CLAUDE_CODE_GATEWAY_HINT_HEADERS` unset — the default behind any
non-Anthropic `ANTHROPIC_BASE_URL`, i.e. every shunt deployment — dropped
`request-class`, `agent-type`, and `prev-tool-durations` from the wire, while
`x-claude-code-agent-id` and `x-claude-code-session-id` kept flowing:

```
seq 190  main turn       x-claude-code-session-id
seq 195  Explore child   x-claude-code-session-id, x-claude-code-agent-id
seq 196  main turn       x-claude-code-session-id
seq 200  Explore child   x-claude-code-session-id, x-claude-code-agent-id
```

That is the §11 gate behaving as described *and* the §5 fallback rule
("when that header is absent, when `x-claude-code-agent-id` is present and
non-blank") being the live path rather than the exceptional one. An operator
who never sets the variable still gets delegated-work detection; what they
lose is `by_type`, the `compaction`/`auxiliary` carve-out, and the compaction
latch.

## Fact (b) — the literal `x-claude-code-agent-type` values

§11 reads the value set from the binary as "`teammate`, a built-in agent id
(`fork`, `Explore`, …), `custom`" and asks for live confirmation. For each
verified mapping below, the `subagent_type` was read back out of the parent's
own `Agent` tool-use block in the captured body, so the mapping is
request-to-request and not inferred.

| `subagent_type` sent by the parent | `x-claude-code-agent-type` on the wire | verified |
|---|---|---|
| `Explore` | `Explore` | ✅ headless + REPL |
| `general-purpose` | `general-purpose` | ✅ |
| `Plan` | `Plan` | ✅ |
| `claude` | `claude` | ✅ |
| `fork` | `fork` | ✅ (needs `CLAUDE_CODE_FORK_SUBAGENT=1`) |
| `marker-finder` (project `.claude/agents/`) | `custom` | ✅ |
| `teammate` | — | ❌ not reachable, see below |

So §11's rule holds as stated: a built-in agent's id travels verbatim,
**case included** (`Explore`, not `explore`; `Plan`, not `plan`), and a custom
agent's own name never appears — `marker-finder` arrived as `custom`. Of the
three `by_type` keys in §11's example config that confirms two as written,
`Explore` and `fork`; `teammate` never reached the wire, so its spelling rests
on the binary's own literal rather than on this capture (both caveats below).

Three details §11's "…" leaves open:

* **`claude` is a built-in id in 2.1.276.** The captured request body carries
  the harness's own "Available agent types for the Agent tool" block, and under
  a fresh `HOME` it lists exactly five: `claude`, `Explore`, `general-purpose`,
  `Plan`, `statusline-setup`. `claude` is the catch-all and it reaches the wire
  as `claude`, so it is a legitimate `by_type` key. (`statusline-setup` was not
  driven — it has no routing interest.) The `Agent` tool's `subagent_type` is a
  free-form string in its `input_schema`, not an enum, so an unknown type is
  rejected at execution rather than refused by the schema.
* **`fork` is gated.** It is absent from the `Agent` tool's accepted
  `subagent_type` set unless `CLAUDE_CODE_FORK_SUBAGENT=1`; with the flag off,
  the tool rejects it and names the available types. With the flag on it
  behaves like any other built-in and sends `x-claude-code-agent-type: fork`.
  Anyone writing a `by_type` example with a `fork` key should know the key is
  inert on a default install.
* **`teammate` is not wire-verified here.** Agent Teams is gated behind
  `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`, and with that flag set a headless
  run still surfaces no team-creation tool — team spawning appears to need an
  interactive team-creation flow this rig did not exercise. The literal is
  present in the 2.1.276 binary's header-derivation function, which returns
  `"teammate"` ahead of the `agent:builtin:` prefix check, so §11's value is
  very likely right; it is simply not something this capture observed.
  **Treat `teammate` as the one unverified key in the table.**

`x-claude-code-prev-tool-durations` came along for the ride and its format
matches §11 exactly — `Name=ms` joined with `;`: `Agent=5`, `Glob=47`,
`Read=3`, `Read=4;Bash=27`.

Not captured: `x-claude-code-compaction` and `x-claude-code-context-compacted`.
Neither the `/compact` command nor an auto-compaction was triggered in these
runs, so §11's one-shot claim about `context-compacted` — the claim PR 1's
session latch is built on — is still binary-derived only.

## Fact (c) — a forced tool-use probe against the judge verdict schema

**This probe sent forced tool use, which is not the shape the pinned code
produces.** The Anthropic Messages API does have a structured-output field —
`output_config.format` — and libsy uses it. At `3ddea9d3`,
`ClassifierResponseFormat::JsonSchema` passes the packaged
`{"type": "json_schema", "json_schema": {…}}` document straight through
(`crates/libsy/src/algorithms/util/classifier_contract.rs:92-95`); it lands on
`LlmRequest.output.response_format`
(`crates/libsy/src/algorithms/llm_class.rs:1654-1655`); and the Anthropic codec
emits it as `output_config.format`
(`crates/switchyard-translation/src/codecs/anthropic/buffered.rs:267-280`). No
tool and no `tool_choice` is constructed anywhere on that path. Forced tool use
is what *this capture* chose to send, so everything below exercises a different
request shape from the pinned translation path.

The two shapes do not meet on the reply side either: an Anthropic `tool_use`
reply decodes to `ContentBlock::ToolCall` (same codec file, ~617) and
`completion_text` (`crates/protocol/src/lib.rs:75-88`) keeps only `Text` blocks,
so `parse_json_verdict`
(`crates/libsy/src/algorithms/util/llm_judge.rs:374-380`) would be handed an
empty string. A forced-tool reply is not merely an alternative encoding of the
production one; on the pinned path it would fail to parse.

The question §10 poses is whether the reply validates against the packaged
schema strictly enough to skip `ClassifierResponseFormat::JsonObject`, which
moves the schema into the prompt and validates locally. What follows answers
that for the forced-tool-use shape only.

The inner schema from each packaged response-format document was sent verbatim
from the pinned libsy revision (`3ddea9d3`) — `EscalationVerdict`
(`prompts/escalation/schema.json`) and `CapabilityClassifierDecision`
(`prompts/capability-classifier/schema.json`) — with each schema's packaged
`prompt.md` as the `system` block. The request shape:

```json
{
  "model": "claude-haiku-4-5-20251001",
  "max_tokens": 1024,
  "system": "<prompts/escalation/prompt.md verbatim>",
  "tools": [{
    "name": "EscalationVerdict",
    "description": "Emit the EscalationVerdict verdict.",
    "input_schema": {
      "type": "object",
      "properties": {
        "escalate": {"type": "boolean", "description": "True when the run is likely doomed without escalation to the strong tier."},
        "reason":   {"type": "string",  "description": "One short sentence naming the trouble pattern, or stating why the run is progressing."}
      },
      "required": ["escalate", "reason"],
      "additionalProperties": false
    }
  }],
  "tool_choice": {"type": "tool", "name": "EscalationVerdict"},
  "messages": [{"role": "user", "content": "<condensed session transcript>"}]
}
```

**On `claude-haiku-4-5-20251001`, both replies validated.** `stop_reason` was
`tool_use`, `content` was exactly one `tool_use` block and nothing else — no
preamble text to strip — and the `input` object satisfied every keyword in the
packaged schema, `additionalProperties: false` included:

```json
{"escalate": true,
 "reason": "same ImportError on repeated test runs with unrelated config file edits between attempts"}
```

```json
{"crux": "Implementing a `--json` flag for an existing `report` subcommand that produces output matching both the text renderer rows AND a pre-existing pytest specification in `tests/test_report_json.py`, with exact object shape validation.",
 "primary_rule": "SUP-5",
 "capability_boundary": "supported",
 "p_solve": 0.78}
```

`primary_rule` and `capability_boundary` landed inside their enums and
`p_solve` inside `[0.0, 1.0]`; no extra property appeared in either object. The
captured replies satisfied every keyword the two schemas use — `type`,
`properties`, `required`, `additionalProperties`, `enum`, `minimum`, `maximum`,
`minLength` — with no keyword left unchecked.

That coverage belongs to the forced-tool-use shape, where the schema travels as
an `input_schema` verbatim. On the pinned `output_config.format` path it is
narrower: the codec runs `strip_anthropic_unsupported_constraints`
(`crates/switchyard-translation/src/codecs/anthropic/buffered.rs:490-511`),
which recursively removes `minimum`, `maximum`, `minLength`, and `maxLength`
from the schema before the request is sent. Those bounds are never exercised in
production, so this capture says nothing about them there — only about the
probe.

### Where this contradicts §10

§10 asks for the fact about "the current Claude models", plural and unqualified.
As a universal it is **false**. `claude-fable-5-1` rejects the request before
any reply exists:

```http
HTTP/1.1 400 Bad Request
content-type: application/json
request-id: req_011CfAAWqhSyN3Wa11UCjjSB
x-gateway-upstream: anthropic
x-gateway-upstream-model: claude-fable-5-1
```
```json
{"type": "error",
 "error": {"type": "invalid_request_error",
           "message": "tool_choice: type \"tool\" and \"any\" are not supported for this model."},
 "request_id": "req_011CfAAWqhSyN3Wa11UCjjSB"}
```

Reproduced twice, minutes apart, on both schemas, with an Anthropic
`request-id` on every response — so it is the model endpoint refusing forced
tool choice, not a gateway or pool artifact. The consequence for PR 5 is
narrow but real: a `[models.router]` entry whose judge resolves to a Fable
target cannot take the forced-tool-use path at all, and what comes back is a
`400` at the first judge call rather than a parse failure.

That distinction decides the blast radius, and libsy's own test speaks to it —
but for a different boundary than the one shunt is building. In
`classifier_stops_on_client_errors_and_records_verdict_fallback`
(`libsy-llm-client/tests/observability.rs`), a `JudgeOutcome::CallFailure`
makes `run_classifier` return `Err`, and the case asserts
`switchyard.classifier_fail_open` is **not** incremented — "client failures and
valid verdicts must not increment switchyard.classifier_fail_open". Only the
`"not json at all"` reply counts as a fail-open, under `reason = "parse_error"`.
That is accurate for what it covers: the test drives
`switchyard_llm_client::run`, the HTTP client runner, whose own `serve`
(`crates/libsy-llm-client/src/run.rs:190-204`) does `call_one(…).await?` and
only then `call.respond(Ok(response))` — the `?` returns `Err` without ever
calling `respond`, aborting the run before the error can reach
`JudgeClassifier::verdict`. On that boundary a failed model call ends the run
and is correctly not a fail-open.

shunt's planned boundary is the other one, and the documented contract there
points the other way. ADR-0005 §1 commits shunt to
`drive(algorithm, request, models, serve)` with shunt authoring its own `serve`
closure (§3), and `drive`'s contract
(`crates/libsy/src/core/algorithm.rs:364-367`) reads: "A failed *model* call
belongs in `respond` — the algorithm may route around it. Returning `Err` from
`serve` aborts the whole run, so reserve it for infrastructure failures." On
that contract-compliant path a `respond(Err(..))` does reach
`JudgeClassifier::verdict`
(`crates/libsy/src/algorithms/util/llm_judge.rs:263-278`), which catches it via
`inspect_err`, records `classifier_fail_open`, folds it into `None`, and runs
the policy fallback — the opposite outcome.

So which of the two a Fable `400` produces depends on how shunt writes its
`serve` closure, and the ADR has not pinned that. Treat it as an open design
decision for PR 5, not a settled fact: the captured `400` is solid, the
behaviour it triggers downstream is still ours to choose. And whether
`tool_choice: {"type": "auto"}` yields a usable verdict there was not tested —
the model was returning the same headerless `429`s by the time this question
came up.

## What this capture does not establish

Stated plainly so a later reader does not over-read the table above.

* **Fact (c) reached two models, not "the current Claude models".** For the
  whole capture window only `claude-haiku-4-5-20251001` answered, plus
  `claude-fable-5-1` for the first few minutes — long enough for it to refuse
  forced tool choice twice before it too started returning `429`s. The other
  nine ids on the gateway (`claude-opus-5`, `claude-sonnet-5`,
  `claude-fable-5`,
  `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`,
  `claude-opus-4-5-20251101`, `claude-sonnet-4-5-20250929`) answered `429`
  `rate_limit_error` with an Anthropic `request-id`, on a bare four-token text
  request as much as on the tool request, through repeated probes over ~30
  minutes. So the fact is **established for one model, contradicted for one
  other, and untested for nine** — untested is not passing.

  **Attribution corrected 2026-09-19.** This bullet originally read those
  `429`s as exhausted pool headroom. The re-capture below finds they carry no
  `retry-after` and no `anthropic-ratelimit-*`, which is the OAuth
  client-shape signature rather than a quota one. The counts above still
  hold; the cause does not — see "Fact (c), re-captured against the pinned
  `output_config.format` shape".
* **The pinned `output_config.format` path was not exercised at all.** Fact (c)
  sent forced tool use, which is not what `ClassifierResponseFormat::JsonSchema`
  produces at `3ddea9d3` (above). §10's question about the production
  structured-output path — whether an `output_config.format` reply validates
  strictly enough to skip `JsonObject` — is therefore still open and needs a
  re-capture against that shape. **Re-captured 2026-09-19; see "Fact (c),
  re-captured against the pinned `output_config.format` shape" below.**
* **The `drive`/`CallModel` failure path was not exercised.** Nothing here shows
  what shunt's own `serve` closure does with a Fable `400`; the libsy test cited
  above covers the HTTP client runner instead. Whether such a call surfaces as a
  classifier failure or as a `classifier_fail_open` with the policy fallback is
  an open decision, not an observation (above).
* **`teammate` was not seen on the wire** (above).
* **The two compaction headers were not exercised** (above).
* **`main` carrying an agent id was never observed.** §5 makes the class
  authoritative so that `main` with an agent id is treated as main traffic;
  nothing in this capture produced that combination, so that branch is
  defensive rather than observed.
* **Fact (b) enumerates 2.1.276's built-ins, not a stable set.** The available
  `subagent_type` values are a client-version property. A future release can
  add or rename one, and a `by_type` key is only ever as good as the client
  sending it.

## Reproducing

The rig is three pieces and no repo change: a config pointing the `anthropic`
provider at a recording tap, the tap, and a driver. The client must be run with
its own `HOME` (agent shadowing, above). Supply the local gateway's
`ANTHROPIC_BASE_URL`, credential, and `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`
through `--settings` or that `HOME`'s `settings.json`: a same-named
`settings.json` `env` entry overrides a shell export. In the first attempt an
existing `ANTHROPIC_BASE_URL` setting won that precedence contest, silently
bypassed the local gateway, and recorded nothing.

## Fact (c), re-captured against the pinned `output_config.format` shape

Second capture, 2026-09-19, answering the question the section above left
open (issue #601): does a reply produced by the **production** request shape —
`output_config.format` carrying the packaged schema, no tool, no
`tool_choice` — validate against that schema strictly enough to skip
`ClassifierResponseFormat::JsonObject`? This section supersedes the two
"not established" bullets above that concern the structured-output path; it
does not amend the ADR.

### The rig, second run

Same three pieces as before, one hop shorter on the client side because no
Claude Code session is involved — the driver is a script that builds the body
the codec would build:

```
driver (python, urllib)  ──http──▶  shunt run (127.0.0.1:31801)
                                      │  auth = "passthrough", provider base_url → tap
                                      ▼
                                    recording tap (127.0.0.1:31802)
                                      │  logs method/path/headers/body, forwards unmodified
                                      ▼
                                    the same hosted shunt 0.44.0 deployment ──▶ api.anthropic.com
                                    (pooled Anthropic subscription credentials)
```

* **shunt**: `0.45.1`, debug build of `d374cc16` (the tip of `main` at the
  time of this note), `shunt run --config …` with the `anthropic` provider's
  `base_url` pointed at the tap. shunt itself does not read or rewrite
  `output_config` on the Anthropic path: nothing in `src/` reads it on the
  Anthropic Messages path. The handlers that do read it map `effort` on other
  paths — the Antigravity adapter (`src/model/antigravity_request.rs`) and the
  OpenAI Responses path (`src/model/responses_request.rs`) — so what the tap
  logged is what the driver sent.
* **Window**: 05:30–05:33 UTC, **30** captured `POST /v1/messages` requests,
  every reply carrying an Anthropic `request-id` and
  `x-gateway-upstream: anthropic`. The schema runs described below are 6 on
  `claude-haiku-4-5-20251001` (three per schema), 20 across the other ten ids
  (two each, one per schema), and the 3 one-off probes under "What a rejection
  of the field itself looks like". This note does not itemize the remainder,
  and it times the bare four-token controls only as "minutes earlier" — not
  precise enough to place them inside or outside this window.
* Credentials, account names, and org/workspace ids are redacted; nothing else
  in the quoted blocks is altered.

### The shape, exactly as the pinned codec emits it

Two corrections to how #601 words the shape, both read off the pinned
revision (`3ddea9d3`) rather than assumed:

1. **On the wire it is `format.schema`, not `format.json_schema`.**
   `encode_anthropic_output_format`
   (`crates/switchyard-translation/src/codecs/anthropic/buffered.rs:451-487`)
   reads the neutral, OpenAI-shaped `{"type":"json_schema","json_schema":{name,
   strict, schema}}` that `ClassifierResponseFormat::JsonSchema` passes
   through, and emits `{"type": "json_schema", "schema": <schema>}`. The
   packaged `name` (`EscalationVerdict`, `CapabilityClassifierDecision`) and
   `strict: true` never reach Anthropic. (The `json_schema.name = "response"`
   wrapper at line 444 is the *decode* direction — Anthropic → neutral — and is
   not sent.)
2. **Four keywords are stripped before sending.**
   `strip_anthropic_unsupported_constraints` (`…/buffered.rs:490-511`)
   recursively removes `minimum`, `maximum`, `minLength`, and `maxLength`.
   `EscalationVerdict` uses none of them, so it goes out unchanged; `CapabilityClassifierDecision` loses `p_solve`'s `[0.0, 1.0]`
   bounds and `crux`'s `minLength: 1`.

The rest of the body follows `JudgeClassifier::build_request`
(`crates/libsy/src/algorithms/util/llm_judge.rs:158-178`): the packaged
`prompt.md` as a single `System` instruction block, which the codec joins into
a **string** `system`; `max_tokens` = `DEFAULT_JUDGE_MAX_OUTPUT_TOKENS`
(`4_096`, `crates/libsy/src/algorithms/util.rs:32`); no `temperature`, no
`stream`, no `thinking`, no `tools`, no `tool_choice`. The client adds
`anthropic-version` and nothing else — in particular **no `anthropic-beta`**
header (`crates/libsy-llm-client/src/backend.rs:164-183`), and the tap logged
none. That holds for the driver-to-tap hop only: the hosted shunt 0.44.0
deployment beyond it resolves a `Credential::ClaudeOauth` and appends
`anthropic-beta: oauth-2025-04-20` before the api.anthropic.com hop
(`src/adapters/anthropic/mod.rs` at `v0.44.0`, the `ClaudeOauth` arm), so
Anthropic did receive a beta header and this capture does **not** establish
whether `output_config.format` is accepted without a beta opt-in. What the tap
logged for the escalation case, verbatim apart from the transcript body:

```json
{
  "model": "claude-haiku-4-5-20251001",
  "system": "<prompts/escalation/prompt.md verbatim>",
  "messages": [{"role": "user", "content": "<condensed session transcript>"}],
  "max_tokens": 4096,
  "output_config": {
    "format": {
      "type": "json_schema",
      "schema": {
        "type": "object",
        "properties": {
          "escalate": {"type": "boolean", "description": "True when the run is likely doomed without escalation to the strong tier."},
          "reason":   {"type": "string",  "description": "One short sentence naming the trouble pattern, or stating why the run is progressing."}
        },
        "required": ["escalate", "reason"],
        "additionalProperties": false
      }
    }
  }
}
```

The two user transcripts are the same condensed sessions as in the forced-tool
capture above. The rest is not a controlled comparison: this run swaps forced
tool use for `output_config.format`, raises `max_tokens` from 1024 to 4096, and
replaces Claude Code's header set with the driver script's.

### Results

| model id | `EscalationVerdict` | `CapabilityClassifierDecision` | outcome |
|---|---|---|---|
| `claude-haiku-4-5-20251001` | 3/3 `200`, **valid** | 3/3 `200`, **valid** | fact established |
| `claude-sonnet-5` | `429` | `429` | untested (below) |
| `claude-opus-5` | `429` | `429` | untested |
| `claude-fable-5-1` | `429` | `429` | untested |
| `claude-fable-5` | `429` | `429` | untested |
| `claude-opus-4-8` | `429` | `429` | untested |
| `claude-opus-4-7` | `429` | `429` | untested |
| `claude-opus-4-6` | `429` | `429` | untested |
| `claude-sonnet-4-6` | `429` | `429` | untested |
| `claude-opus-4-5-20251101` | `429` | `429` | untested |
| `claude-sonnet-4-5-20250929` | `429` | `429` | untested |

**On `claude-haiku-4-5-20251001`, all six replies validated — against the
schema as sent and against the full packaged schema, bounds included, with
no `JsonObject` fallback.** Each reply had `stop_reason: end_turn` and a
`content` array of exactly one `text` block: no preamble, no trailing
commentary, no Markdown fence (`strip_json_fence` was a no-op on all six).
Fed through the pinned parse path — `completion_text` concatenates the `Text`
blocks, `parse_json_verdict` trims and deserialises — every reply satisfied
`type`, `properties`, `required`, `additionalProperties: false`, and `enum`,
and the two bounds the codec had stripped held anyway: `p_solve` came back as
`0.82`, `0.78`, `0.82`, and `crux` was 215–350 characters. No unchecked
keyword remained. One reply of each, verbatim:

```json
{"escalate": true, "reason": "same ImportError 4 times while editing unrelated config files instead of addressing the missing import"}
```

```json
{"crux": "Implement a --json flag for the report subcommand that produces output matching both the text renderer's rows and the exact object shape defined in the test file tests/test_report_json.py, verified by passing pytest", "primary_rule": "SUP-1", "capability_boundary": "supported", "p_solve": 0.82}
```

(`req_011CfCFNNYRbwDibAVNwfM9Q`, `req_011CfCFPyMiACTLCndL92Bki`, then two
repeats each: `req_011CfCFVxR5Gvxp17aW8n4c6`, `req_011CfCFW9GgRjDbzpiwERYKg`,
`req_011CfCFWPB6fhMDFTMfcvWao`, `req_011CfCFWaP3HZkB8Mi9oXhpc`.) Output was
33–37 tokens for the escalation verdict and 79–103 for the classifier
decision, against a `max_tokens` of 4096 — the budget is not close to binding.

### The rejection shape — and it is not the field being rejected

Every one of the other ten ids answered both schemas the same way, twenty
times out of twenty:

```http
HTTP/1.1 429 Too Many Requests
content-type: application/json
request-id: req_011CfCFNiKZC5o89FDCe9BLi
x-gateway-model: claude-sonnet-5
x-gateway-upstream: anthropic
x-gateway-upstream-model: claude-sonnet-5
x-should-retry: true
x-shunt-account: <redacted>
```
```json
{"type": "error",
 "error": {"type": "rate_limit_error", "message": "Error"},
 "request_id": "req_011CfCFNiKZC5o89FDCe9BLi"}
```

Three things about that reply decide what it is. It carries **no
`retry-after` and no `anthropic-ratelimit-*` header** — checked across all
twenty. The same ten ids returned the identical status and body to a bare
four-token text request with no `output_config` at all, minutes earlier
through the same deployment. And `claude-haiku-4-5-20251001` returned `200`
to the very same body during the same window, while an interactive Claude
Code session on `claude-fable-5-1` was completing turns through the same
deployment. That is the signature shunt labels `client-shape-rejection`
(`rate_limit_kind`, `src/adapters/anthropic/mod.rs:1101-1112`) — a local
diagnostic label for the response shape, not an observation of Anthropic's
reason for sending it. The causal reading behind that label comes from the
auto-mode classifier module's comparison
(`src/adapters/anthropic/auto_mode_classifier.rs`), which varied only the
`system` field: relayed unmodified `429` 15/15, a neutral sentence prepended
`429` 10/10, Claude Code's identity prepended `200` 1/1. The two `429` rows
carry the weight; the single `200` shows the block is sufficient, not how
reliably. That comparison also varied a *classifier* request, and this capture
ran no identity-marker control of its own on a judge request, so the
attribution carries over as the best-supported explanation rather than a
verified one. A judge request has no first-party identity marker in its
`system` by construction, and on an OAuth-pooled deployment it draws the `429`
on every non-haiku Claude id tested
**whether or not the well-formed `output_config.format` is present** — the
bare request with no `output_config` above drew the same `429` from all ten.
Whether the gate fires ahead of schema validation was not tested here: no
malformed schema was ever sent to a gated id. There is no reply to validate,
and nothing in the exchange is a verdict on the field.

This also corrects the attribution in the 2026-09-18 section above, which
read the same `429`s as "no pool headroom". They carried the same headerless
signature then, and shunt's own triage treats a quota limit as one carrying
`retry-after` or `anthropic-ratelimit-*` (`rate_limit_kind`) — a discriminator
this capture applies, not one it proves. The independent reason to doubt
exhaustion is concurrent success: `claude-haiku-4-5-20251001` answered `200`
through the same pool in the same window. Why it is admitted is not
something this capture explains — it is observed, not understood — and the
one ordering clue is from the earlier capture: `claude-fable-5-1` answered a
forced-`tool_choice` request with a `400 invalid_request_error` rather than
this `429`, which is consistent with request validation running ahead of the
gate. If so, a schema-keyword `400` (next) would still surface on a gated
model, while a well-formed judge request gets the `429`.

A diagnostic arm that would prepend Claude Code's identity sentence to
`system`, purely to get the ten ids past the gate and observe their
structured-output behaviour, was prepared and **not run**. Sending a
first-party identity marker on a judge request is impersonation of the
client — the line PR #331 drew when it declined to do this for anything but
the auto-mode classifier — and this rig did not cross it.

### What a rejection of the field itself looks like

No model rejected `output_config.format`. To record the shape a judge target
does produce when the *schema* carries something the grammar refuses, the
packaged `CapabilityClassifierDecision` was sent to
`claude-haiku-4-5-20251001` **without** the codec's strip:

```http
HTTP/1.1 400 Bad Request
request-id: req_011CfCFX7pQ534gNu2T3Tj1Y
x-gateway-upstream: anthropic
x-gateway-upstream-model: claude-haiku-4-5-20251001
```
```json
{"type": "error",
 "error": {"type": "invalid_request_error",
           "message": "output_config.format.schema: For 'number' type, properties maximum, minimum are not supported"},
 "request_id": "req_011CfCFX7pQ534gNu2T3Tj1Y"}
```

So the strip of `minimum`/`maximum` is load-bearing — without it the
production classifier request is a `400` at the first call. The other two
stripped keywords are a different story: the same schema with `minimum` and
`maximum` removed but `crux.minLength: 1` kept was accepted
(`200`, `req_011CfCFZ39uvrCw4fqQpFk6f`), as was one with `maxLength: 2000`
added to `crux` (`200`, `req_011CfCFamFkcja24zpmTrPuH`). One request each,
one model, so read it narrowly: the endpoint *accepted* string-length bounds
it is documented by the codec as not supporting; whether it *enforces* them
was not tested (`crux` was non-empty either way). The codec's strip is wider
than this endpoint required on this day, and harmless — the packaged schemas'
`minLength` is only ever `1`.

### What this capture establishes, and does not

* **Fact (c) on the production shape is established for one model and
  untested for ten.** `claude-haiku-4-5-20251001`: 6/6 replies parse on the
  pinned path and validate against the packaged schemas with
  `additionalProperties: false`, no fallback needed. `claude-sonnet-5`,
  `claude-opus-5`, `claude-fable-5-1`, `claude-fable-5`, `claude-opus-4-8`,
  `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`,
  `claude-opus-4-5-20251101`, `claude-sonnet-4-5-20250929`: **untested**, not
  failing — none of them evaluated the field. §10's "the current Claude
  models" is still not a universal this note can back.
* **No model rejected `output_config.format`.** The one model that reached it
  accepted it — under the OAuth beta header the hosted deployment adds, so
  acceptance without a beta opt-in is not something this capture shows; the
  only field-level rejection observed is Anthropic refusing `minimum`/`maximum`
  on a `number`, which the pinned codec already strips.
* **The blocker for the other ten matches the OAuth client-shape gate, and is
  neither headroom nor the field.** The two negatives are directly evidenced;
  the gate attribution is the inference above. For PR 4 (#594) this is the
  concrete first failure a `[models.router]` judge target hits on an
  OAuth-pooled deployment with any of those ids: a headerless
  `429 rate_limit_error` with an Anthropic `request-id`, on the first call,
  with no structured-output reply to inspect. Whether that `429` precedes
  schema validation is untested, as above — a schema-keyword `400` reaching
  one of these ids first is not ruled out.
  Whether that surfaces as a classifier failure or as `classifier_fail_open`
  is the same open `serve` decision recorded above; it now has two inputs
  (the fable `400` and this `429`), not one.
* **The API-key path was not exercised.** Every credential on this rig is a
  pooled subscription OAuth token, so whether an `x-api-key` credential lets
  those ten ids evaluate the field is unknown here, not implied either way.
* **`minLength`/`maxLength` enforcement was not tested** (above); only
  acceptance was.

### Reproducing, second run

Same rig as "Reproducing" above minus the Claude Code client: the driver
POSTs the body shown under "The shape" to the local gateway with the
deployment's bearer token and `anthropic-version: 2023-06-01`, reads
`content[].text` back, and validates the parsed object against the packaged
`schema.json` with the same keyword-by-keyword checker used for the forced
tool-use capture. Build the schema from the pinned checkout under
`~/.cargo/git/checkouts/`, not from a copy, and apply the four-keyword strip
before sending or the classifier request is the `400` above.
