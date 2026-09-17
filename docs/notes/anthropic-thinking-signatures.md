# Anthropic thinking-block signatures

External-behaviour note for `api.anthropic.com`. It records what Anthropic
documents about the `signature` on a `thinking` block, because shunt mints
signatures of its own and has to know which of them may be forwarded back.

## What the signature is

Not a checksum over the thinking text — the payload itself. Anthropic's
[thinking docs][thinking] describe it as "an encrypted copy of the full
reasoning that you pass back unchanged", and say the server "decrypts the
`signature` to reconstruct the original thinking for prompt construction". A
block whose `thinking` field is empty is still complete, because the signature
carries the reasoning — that is the `display: "omitted"` shape, which [the same
page][thinking] lists as the default on Claude Fable 5.1, Mythos 5.1, Fable 5,
Mythos 5, Opus 5, Sonnet 5, Opus 4.8, and Opus 4.7. (Named rather than
generalised to "current models": the doc gives a list, and the list is what is
citable.)

That rules out one tempting reading: there is no mode in which a signature is
advisory. A value the server cannot decrypt is not a weaker signature, it is not
a signature.

## What happens to one Anthropic did not issue

Two documented failures, and they are different errors:

* **Altered block** — 400 `invalid_request_error`, message containing
  ``` `thinking` or `redacted_thinking` blocks in the latest assistant message
  cannot be modified ```. Raised when the assistant turn sent back differs from
  the one the API returned.
* **Unverifiable signature** — 400 `invalid_request_error`, message beginning
  ``` messages.{i}.content.{j}: Invalid `signature` in `thinking` block ```.
  [Troubleshooting][troubleshooting] is explicit about which half of that
  applies here: when the message *stops* after that phrase, "the signature
  itself didn't verify: it was truncated, altered, or **sent back empty**". The
  longer form of the same message, which continues "The block is bound to a
  different conversation", is the prefix check and a different cause.

Two limits on how far that generalises, both worth stating because this note
exists to be trusted later:

1. The signature-verification error is documented against **Claude Fable 5.1**,
   and the prefix half of it is "enforced for new accounts created on or after
   August 31, 2026, and for any request that sets
   `thinking.block_binding.prefix_mismatch_behavior`".
2. A block a model merely *cannot read* — one from a newer Claude — is not this
   case at all: "the API drops a block the current model can't read, without an
   error and without billing it."

So the documented outcome for a signature that was never Anthropic's is a 400 on
at least one current model, and undefined elsewhere. **Not measured against a
live endpoint here.** What is not in doubt is the direction: forwarding a value
shunt invented into a field the server decrypts cannot be correct under any of
the behaviours above.

## The documented remedy is the fix shunt implements

For a history carrying an invalid block, the troubleshooting page says to
"strip every `thinking` and `redacted_thinking` block from the history … leave
each turn's other blocks in place". `src/adapters/anthropic/thinking.rs` does
exactly that, narrowed to the blocks shunt can prove are its own.

One case needs more than removing a block: an assistant turn holding *nothing*
but foreign thinking. Emptying its `content` is rejected, so the whole message
goes. That is not this module's invention — `inbound_responses::messages_request`
has always done it on its own path, with the rule stated in `flush`: "Anthropic
rejects an assistant message holding nothing but a thinking block", at any
position and not only a trailing one. And the shape is common rather than
degenerate: `src/model/responses.rs` opens an empty thinking block purely to
carry the round-trip signature, so a reasoning-only turn is exactly what the
Responses path produces when a turn has no text and no `tool_use`.

`redacted_thinking` is deliberately *not* stripped, despite the quote naming it.
It carries `data` and no signature, so there is no predicate that could tell a
foreign one from a genuine one — and no shunt path mints one toward an
Anthropic-protocol client. The only producer, `inbound_responses::reasoning`,
reconstructs blocks shunt itself encoded from genuine Anthropic ones, for the
inbound Codex endpoint. Stripping the type wholesale would break that round trip
to fix a leak that does not exist.

The narrowing matters: Anthropic's advice is aimed at a client that knows its
history is broken, while shunt is a gateway that cannot verify a genuine
signature and must not drop one. `crate::model::thinking_signature` is therefore
written as "what shunt minted", never as "what looks valid".

## Which signatures shunt mints

Every non-Anthropic provider surfaced through the Messages protocol has to put
something in the field, because Claude Code round-trips assistant blocks
verbatim. Three do, and `crate::model::thinking_signature` owns all three so the
strip cannot fall out of step with them.

| Producer | Value | Why it is not Anthropic's |
|---|---|---|
| Responses / Codex | base64url of `{"id", "enc"}` | shunt's own round-trip envelope for a Responses `reasoning` item |
| Gemini | `gemini_thinking` | Gemini issues no signature; the literal exists to make the block well-formed |
| Cursor | `""` | the agent stream carries reasoning text and nothing signature-shaped |

The empty string is the one to watch, because Anthropic's own streaming shape
uses it too: [the thinking page][thinking]'s worked event sequence opens the
block as `{"type": "thinking", "thinking": "", "signature": ""}` and fills it
with a later `signature_delta`. That is a *response* mid-stream, though, and this
strip reads *requests*, where the same value is unusable — "sent back empty" is
named above as a verification failure. So stripping it is right, and a reader who
meets `"signature": ""` in an SSE capture should know why the value appears on
both sides without that being a contradiction.

## Why the reverse direction was already handled

The Responses translator has dropped foreign signatures since it was written:
`decode_reasoning_signature` returns `None` for a genuine Anthropic signature and
the block goes no further, because "the Responses backend rejects reasoning it
never issued". `src/model/inbound_responses/reasoning.rs` states the matching
rule for its own direction — "Anthropic would reject a thinking block it never
signed". Only the Anthropic *outbound* path lacked the mirror, which is what
`thinking.rs` adds.

[thinking]: https://platform.claude.com/docs/en/build-with-claude/thinking
[troubleshooting]: https://platform.claude.com/docs/en/build-with-claude/thinking-troubleshooting
