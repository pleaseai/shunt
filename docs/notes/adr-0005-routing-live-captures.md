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

The second shunt hop is only how this machine reaches Anthropic; every reply
quoted below carries Anthropic's own `request-id` and `cf-ray`, so the origin
of a `200`, a `400`, or a `429` is never in doubt.

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
  agents in scope were the built-ins plus one deliberately-custom project
  agent. This matters — the first attempt at fact (b) reported `custom` for
  `Explore`, because the real `HOME` has a `~/.claude/agents/Explore.md` that
  shadows the built-in of that name.
* **Model**: `claude-haiku-4-5-20251001`, the only Claude id with pool headroom
  during the capture window (see "What this capture does not establish").
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

Two of the four `custom` rows predate the throwaway `HOME` and came from the
shadowed `~/.claude/agents/Explore.md` — which is a custom agent, so `custom`
is the right value for them. The other two are the project's `marker-finder`
(fact (b)).

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
(`fork`, `Explore`, …), `custom`" and asks for live confirmation. Each row
below is one drive that spawned exactly that agent, with the `subagent_type`
read back out of the parent's own `Agent` tool-use block in the captured body,
so the mapping is request-to-request and not inferred.

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
agent's own name never appears — `marker-finder` arrived as `custom`. The
`by_type` keys in §11's example config are therefore correct as written.

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
  interactive surface this rig did not drive. The literal is present in the
  2.1.276 binary's header-derivation function, which returns `"teammate"`
  ahead of the `agent:builtin:` prefix check, so §11's value is very likely
  right; it is simply not something this capture observed. **Treat `teammate`
  as the one unverified key in the table.**

`x-claude-code-prev-tool-durations` came along for the ride and its format
matches §11 exactly — `Name=ms` joined with `;`: `Agent=5`, `Glob=47`,
`Read=3`, `Read=4;Bash=27`.

Not captured: `x-claude-code-compaction` and `x-claude-code-context-compacted`.
Neither the `/compact` command nor an auto-compaction was triggered in these
runs, so §11's one-shot claim about `context-compacted` — the claim PR 1's
session latch is built on — is still binary-derived only.

## Fact (c) — a forced tool-use reply against the judge verdict schema

The Anthropic Messages API has no `response_format`, so libsy's
`ClassifierResponseFormat::JsonSchema` maps onto forced tool use: the packaged
verdict schema becomes a tool's `input_schema` and `tool_choice` names it. The
question §10 poses is whether the reply then validates against that schema
strictly enough to skip `ClassifierResponseFormat::JsonObject`, which moves the
schema into the prompt and validates locally.

Both packaged schemas were sent verbatim from the pinned libsy revision
(`3ddea9d3`) — `EscalationVerdict` (`prompts/escalation/schema.json`) and
`CapabilityClassifierDecision`
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
`p_solve` inside `[0.0, 1.0]`; no extra property appeared in either object.
Validation covered every keyword the two schemas use — `type`, `properties`,
`required`, `additionalProperties`, `enum`, `minimum`, `maximum`, `minLength` —
with no keyword left unchecked.

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
target cannot take the forced-tool-use path at all, and the failure is a `400`
at the first judge call rather than a parse failure that `fail_open` would
absorb. Whether `tool_choice: {"type": "auto"}` yields a usable verdict there
was not tested — the model had no pool headroom left by the time the question
came up.

## What this capture does not establish

Stated plainly so a later reader does not over-read the table above.

* **Fact (c) reached two models, not "the current Claude models".** For the
  whole capture window only `claude-haiku-4-5-20251001` had pool headroom, plus
  `claude-fable-5-1` for the first few minutes — long enough for it to refuse
  forced tool choice twice before it too ran out. The other nine ids on the
  gateway (`claude-opus-5`, `claude-sonnet-5`, `claude-fable-5`,
  `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`,
  `claude-opus-4-5-20251101`, `claude-sonnet-4-5-20250929`) answered `429`
  `rate_limit_error` with an Anthropic `request-id`, on a bare four-token text
  request as much as on the tool request, through repeated probes over ~30
  minutes. So the fact is **established for one model, contradicted for one
  other, and untested for nine** — untested is not passing.
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
its own `HOME` (agent shadowing, above) and with
`CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` supplied through `--settings` or that
`HOME`'s `settings.json` — an exported environment variable loses to a
`settings.json` `env` entry, which is how the first attempt silently bypassed
the gateway entirely and recorded nothing.
