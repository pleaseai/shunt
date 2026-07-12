---
name: shunt-otel-privacy-claim-rot
description: shunt's OTel export module documented a privacy gate (include_session_id) that was never actually implemented; grep config field names across the whole src tree, not just the module that defines them.
metadata:
  type: project
---

In the `chatbot-pf`/pleaseai `shunt` repo (LLM gateway), the branch adding OpenTelemetry export
(`src/telemetry.rs`, `src/config.rs` `OtelConfig`) introduced `include_session_id: bool` (default
`false`) with doc-comments in three places (`src/telemetry.rs` module doc, `src/config.rs`
`OtelConfig` struct doc, and the `include_session_id` field doc) all claiming the client session id
"rides on spans only when `include_session_id` is set" / is "withheld unless" opted in.

`grep -rn "include_session_id" src/` showed the field is read nowhere outside `src/config.rs`
itself (struct def, default, and a unit test asserting the default is `false`). The `proxy_request`
span that actually carries `session_id` is built in `src/proxy.rs` (untouched by the otel diff) and
sets the field **unconditionally** from the `x-claude-code-session-id` header, with no reference to
`config.otel` at all. So once traces are exported via OTLP, the session id always rides along —
the documented opt-in privacy gate is pure vapor. `docs/running.md` and `shunt.toml.example` repeat
the same false claim (`# include_session_id = false # (default) withhold the client session id`).

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
`/private/tmp/otel-src/` and turned out accurate, unlike the session-id claim.
