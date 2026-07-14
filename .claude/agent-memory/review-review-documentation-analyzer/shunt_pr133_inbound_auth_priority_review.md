---
name: shunt-pr133-inbound-auth-priority-review
description: PR #133 (branch amondnet/130) — inbound gate-token priority (header > Bearer > x-api-key) extended to /v1/messages; docs review method and a pre-existing (non-doc) wording quirk found along the way.
metadata:
  type: project
---

PR #133 (issue #130, branch `amondnet/130`) extended `InboundAuth` so the
`/v1/messages` inference gate — not just `GET /v1/models` discovery — accepts
the gate token via `x-shunt-token` (or configured header), `Authorization:
Bearer`, or `x-api-key`, with that exact priority when multiple slots carry
valid tokens (`src/auth/inbound.rs::authenticate_client`, renamed from
`authenticate_discovery`). On a match, `check_inbound_auth` in `src/proxy.rs`
now strips `authorization` and `x-api-key` in addition to the configured
header before forwarding.

Docs touched: `docs/m4-inbound-auth.md`, `docs/running.md` (§5.9),
`shunt.toml.example`, and the en/ko/ja/zh-cn site guides
(`shared-gateway.md`, `connect-claude-code.md`, `anthropic-multi-account.md`)
+ reference (`configuration.md`, `troubleshooting.md`). Reviewed the full
`git diff main...HEAD` against `src/auth/inbound.rs` + `src/proxy.rs` +
`src/discovery.rs`, ran `cargo test --test inbound_auth` and
`cargo test --lib auth::inbound` (all green, 22 tests), and spot-checked
translated anchor slugs (`#inbound-client-tokens` → ja
`#インバウンドのクライアントトークン`, ko `#인바운드-클라이언트-토큰`, zh-cn
`#入站客户端-token` — headings match, no dead English fragments). Result:
**zero findings** — every doc claim (priority order, header-stripping
boundary, `AuthMode` variant names, JSON error-body shape, §5.9 section
number, `shunt.toml.example` keys) checked out against the code and tests.

Non-doc observation (did not report, out of scope for a docs-only review):
`src/discovery.rs`'s 401 message (unchanged by this PR) lists slots as
"`{header}`, `x-api-key`, or `Authorization: Bearer`" while `src/proxy.rs`'s
(updated) message lists "`{header}`, `Authorization: Bearer`, or
`x-api-key`" — cosmetic wording-order mismatch between the two 401 bodies,
not a functional bug (both call the same `authenticate_client` with the
correct priority) and not contradicted by any doc. Worth a one-line
mention if a future PR touches `discovery.rs`'s error text, but not
worth flagging in a pure documentation-accuracy pass since no markdown
claims the discovery order specifically.

See also [[shunt_i18n_docs_structure]] for the general i18n review method
this PR reused successfully.
