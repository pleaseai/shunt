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
carries the reasoning; `display: "omitted"` is the default on current models and
returns exactly that shape.

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

The empty string is the one to watch, because Anthropic also opens a streaming
thinking block with `"signature": ""` before the `signature_delta` fills it. In a
*request* that shape is still unusable — "sent back empty" is named above as a
verification failure — so stripping it is right either way, but a reader
comparing this table against a live SSE capture should know why the value
appears on both sides.

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
