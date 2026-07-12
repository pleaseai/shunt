---
name: shunt-otel-privacy-claim-rot
description: shunt's OTel export module first documented a privacy gate (include_session_id) that the initial diff never wired up; the gate was implemented during PR review, so treat this as a resolved example of the "grep config field names across the whole src tree" lesson.
metadata:
  type: project
---

In the `chatbot-pf`/pleaseai `shunt` repo (LLM gateway), the branch adding OpenTelemetry export
(`src/telemetry.rs`, `src/config.rs` `OtelConfig`) introduced `include_session_id: bool` (default
`false`) with doc-comments in three places (`src/telemetry.rs` module doc, `src/config.rs`
`OtelConfig` struct doc, and the `include_session_id` field doc) all claiming the client session id
"rides on spans only when `include_session_id` is set" / is "withheld unless" opted in.

**On the initial diff the gate was dead code.** `grep -rn "include_session_id" src/` showed the
field was read nowhere outside `src/config.rs` (struct def, default, and a unit test asserting the
default is `false`). The `proxy_request` span that carries `session_id` is built in `src/proxy.rs`,
which the first otel diff left untouched — it set the field unconditionally from the
`x-claude-code-session-id` header with no reference to `config.otel`, so once traces were exported
the session id always rode along. The documented opt-in was a privacy/security guarantee the code
did not enforce.

**Resolution (do not re-flag as unimplemented):** the gate was implemented during PR review. As of
PR #64, `src/proxy.rs` reads the decision via `crate::telemetry::withhold_session_id()`, which is
pinned once at startup from the `[otel]` config (`WITHHOLD_SESSION_ID` in `src/telemetry.rs`) rather
than from the hot-swappable per-request config — because the OTLP exporter is built once and never
rebuilt on reload, the privacy decision that governs its output must be fixed for the process
lifetime. So the current tree DOES enforce the opt-in; a future agent should verify `withhold_session_id`
is still wired before assuming otherwise, not repeat the old "pure vapor" claim.

**Why:** This is the highest-value class of finding for this agent role — a comment stating a
privacy/security guarantee that the code does not enforce is much worse than a stale wording nit.
It's also easy to miss because the field *exists*, has a sensible default, and is unit-tested for
its default value — everything *looks* wired up except the one thing that matters (an actual call
site reading it before adding the span field).

**How to apply:** Whenever a diff adds a config field whose doc-comment claims it gates or redacts
some data (privacy, security, cardinality claims), grep the field name across the *entire* `src/`
tree (not just the file/module where it's defined) before trusting the claim. If the only hits are
the definition, its default, and a test of the default, the gate is dead code and the doc-comment
is a factually incorrect (not just stale) comment — flag at Critical severity. See also: verify
other cross-cutting comment claims (subscriber-layer composition order, exporter endpoint
resolution semantics) against the actual dependency source when the claim is about a third-party
crate's non-obvious behavior — in this diff those (reload-layer type pinning, EnvFilter gating all
layers, OTLP `.with_endpoint()` being used verbatim vs. env-var auto-append) were all checked
against the vendored `opentelemetry_sdk`/`opentelemetry-otlp` source at
`/private/tmp/otel-src/` and turned out accurate, unlike the original session-id claim.
