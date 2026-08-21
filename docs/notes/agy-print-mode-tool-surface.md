# agy print mode exposes tools it cannot service

**Last Updated:** 2026-08-21 — see [issue #413](https://github.com/pleaseai/shunt/issues/413)

---

## The finding

`agy --print --output-format stream-json` advertises its **full** interactive
tool surface in the `init` event, including tools that only work inside a live
multi-agent session:

- `send_message` — hand a message to another agent's inbox
- `manage_inbox`, `manage_subagents`, `invoke_subagent`, `define_subagent`
- `ask_question`, `ask_permission`, `ask_custom_permission`
- `schedule`, `wait`, `wait_5_seconds`

Print mode registers no inbox and no peer agents, so a model that takes one of
these up gets a failure the CLI treats as fatal for the whole run.

## How it surfaced

Any subagent-style prompt ("report your findings back to the main agent") is
enough for Gemini to deliver its reply through `send_message` instead of, or in
addition to, the response body. The call fails and `agy` emits a terminal
result:

```json
{"event":"result","result":{"status":"ERROR","response":"",
 "error":"error executing cascade step: CORTEX_STEP_TYPE_GENERIC: recipient \"main\" not found"}}
```

Crucially this arrives **after** the answer has streamed in full. Recipients
observed: `main`, `team-lead`, `user`.

Reproduce in one request:

```bash
curl -sN -X POST http://localhost:3001/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"claude-gemini-3.7-flash-via-antigravity","max_tokens":4096,"stream":true,
       "system":"You are a subagent. When you have completed your task, send your final report back to the main agent (recipient: main).",
       "messages":[{"role":"user","content":"Count from 1 to 5, then report the result back to main."}]}'
```

## What shunt does about it

`Translator::on_line` (`src/adapters/antigravity/stream.rs`) treats a non-`SUCCESS`
terminal result as `AgyEnd::Success` when the error has the
`recipient "<name>" not found` shape **and** assistant text was already
streamed. Everything else keeps failing the turn.

Both conditions matter:

- Without the shape check, every late failure would be normalised — including
  `--print-timeout` expiry, `Eligibility check failed: UNAVAILABLE (code 503)`,
  and `context canceled`, all of which do leave a partial answer.
- Without the text check, a run whose only output lived inside the undelivered
  message would be reported as a successful empty turn.

## Why not fix it at the spawn site

Preferable, but unavailable on agy 1.1.17: `agy --help` has no tool allow/deny
flag, and `~/.gemini/antigravity-cli/settings.json` carries only a permission
allowlist — no `tools` / `disabledTools` key. Denying permission would leave the
tool advertised and convert its use into a different terminal failure.

The matcher is therefore a compatibility shim keyed on an upstream error string.
**Drop it** once `agy` gains a way to withhold tools at spawn time, and prefer
withholding the whole list above rather than only `send_message`.

## Ruled out

These were tested against the same symptom and are not causes: four concurrent
gateway streams (all completed cleanly), heavy tool use, long generations, and
quota — the 429s in `~/.gemini/antigravity-cli/log/` are on `loadCodeAssist` and
`setUserSettings` metadata calls, which the CLI recovers from.
