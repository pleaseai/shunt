# Cross-vendor usage bars — follow-up issue drafts

Drafts only, not filed. M-A (the `GET /api/oauth/usage` synthesizer) is fully specified in
`DESIGN.md`, but — per that document's "Status" line and Boundaries — is **not**
self-approving: it still needs (a) the pre-implementation CLI-reachability check in its
"Precondition" section run and recorded, and (b) explicit maintainer sign-off on its new
`[server.oauth_usage]` config key and its non-standard (bind-topology-gated, not
credential-matched) auth model. File a tracking issue for M-A covering those two gates
before work starts, even though the technical contract itself needs no further design
iteration.

Ollama and Gemini are deliberately **not** drafted as issues below — see the note at the end.

---

## proposal: extend `GET /usage` with per-provider windows (M-B fallback surface)

**Problem.** Claude Code's native usage UI cannot show cross-vendor bars: its `limits[]`
renderer only draws entries whose `scope.model.display_name` is in a GrowthBook remote
feature flag (`tengu_usage_overage_included_models`) that Anthropic controls server-side —
confirmed today to be exactly `["Fable", "Fable 5"]`, independent of `ANTHROPIC_BASE_URL` or
anything shunt's response contains (see `DESIGN.md`, M-B). shunt cannot make a Codex or Grok
bar appear in that specific UI no matter what `/api/oauth/usage` returns.

Separately, `GET /usage` ([m12](../../m12-client-usage-endpoint.md)) already has a latent
correctness gap worth fixing regardless of the above: its aggregate blends `ClaudeOauth` and
`ChatgptOauth` accounts into one pool-wide `5h`/`7d`/`fable` set
(`src/usage.rs:174-177`), so a saturated Codex account can make a healthy Claude pool report
near-zero headroom, and vice versa, once a deployment runs both backends.

**Evidence.** `src/usage.rs:174-177` (provider filter, no per-provider split);
`docs/m12-client-usage-endpoint.md` ("Contrast with GET /admin/pool" table — per-account
detail deliberately dropped, but nothing distinguishes providers either); recon probe
(`renders_extra_limits: "no"`, GrowthBook allowlist evidence).

**Proposed change.** Add an additive `providers` object to the existing `GET /usage`
response, one entry per configured provider name (see "Open questions" below — this is now
resolved in favor of provider name over `AuthMode`), each with the same `{status, windows}`
shape as the existing pool-wide aggregate, computed from that provider's own snapshot slice
via the existing `usage::window_status`. The existing top-level `pool` key's *computation*
is unchanged (backward compatible); its documentation is strengthened to state plainly that
it is a coarse, cross-backend signal now that `providers` gives per-backend accuracy. Gate
behind the existing `[server.usage]` opt-in — no new config table.

**Constraints.**
- Must not add any new account-identifying field (name, priority, disabled, threshold,
  headroom, cooldown) — same sanitization invariant `GET /usage` already enforces.
- Must not attempt to make Codex/xAI/Kimi bars appear inside Claude Code's own native UI —
  that path is blocked by Anthropic's client-side allowlist, not by shunt (see Non-goals).

**Non-goals.**
- Rendering cross-vendor bars in Claude Code's native `/usage` view. Blocked client-side;
  not shunt's to fix. If Anthropic's `tengu_usage_overage_included_models` allowlist ever
  widens, that is a reason to revisit — not something this ticket should design around.
- Any new usage *source* (that is M-C's scope, tracked separately per vendor).

**Open questions.**
- ~~Should `providers` key by the config's provider name or by `AuthMode`?~~ **Resolved** in
  `DESIGN.md` (M-B): provider name. Keying by `AuthMode` would silently re-blend a
  deployment running two `claude_oauth` providers back into one bucket, reintroducing this
  same ticket's bug one level down; provider names are operator-chosen config labels, not
  account secrets, and this endpoint already requires an authenticated `[server.auth]`
  caller.
- Does the admin dashboard's pool table (M9) want the same per-provider rollup, or does its
  existing per-account table already subsume this?

---

## feat(codex): poll the Codex/ChatGPT usage endpoint for out-of-band reconciliation

**Problem.** shunt's Codex quota tracking today is header-only (`note_codex_quota`,
`src/accounts.rs:458-514`) — it only ever knows what a shunt-proxied response told it. Usage
the operator racks up through their *own* interactive Codex CLI, running in parallel outside
shunt, never reconciles into the pool's view, the same gap [M8](../../m8-anthropic-multi-account.md)'s
Claude usage poller (`src/usage_poll.rs`) exists to close for the Claude side.

**Evidence.** OpenAI's own `codex-rs` source (`backend-client/src/client/rate_limit_resets.rs`)
confirms a real, actively-polled endpoint backing the Codex CLI's own `/status` display:
`GET {base}/wham/usage` (ChatGPT-OAuth path style) or `GET {base}/api/codex/usage` (API-key
path style), authenticated with the same Bearer token + `ChatGPT-Account-Id` header shunt
already injects for its Codex accounts. Response shape (from OpenAI's own generated OpenAPI
models) carries `primary_window`/`secondary_window` (`used_percent`, `reset_at`,
`reset_after_seconds`, `limit_window_seconds`) matching the two buckets shunt's header path
already tracks, plus `additional_rate_limits[]` (per-model/per-feature caps shunt does not
track at all today).

**Constraints.**
- Same shape as the existing Claude usage poller: opt-in, boot-decided (not reload-toggled),
  and a `resolve-live-token → fetch → note_*` cycle reusing `AccountPool`'s existing
  ingestion pattern rather than a new one.
- The endpoint is undocumented by OpenAI (community-confirmed only, e.g.
  `openai/codex#10869`); the poller must degrade the same way the Claude poller does on
  failure — `tracing::debug!`, never surfaced to any presentation layer, never poisons
  existing header-derived state.

**Non-goals.**
- `additional_rate_limits[]` — no existing bucket type fits per-model/per-feature caps;
  designing that shape is a separate, larger question than "reconcile the two windows shunt
  already tracks." File separately if pursued.
- Any change to the header-based ingestion path (`note_codex_quota`) — this is additive
  reconciliation, not a replacement.

**Open questions.**
- `wham/usage` vs `api/codex/usage`: does shunt pick the path by the account's own auth mode
  (mirrors how it already picks ChatGPT-vs-API-key headers today), or does it need to try
  both?
- Poll interval: reuse `[server.pool].usage_refresh_seconds` (already exists, already
  floor-clamped to 60s) for both backends, or does Codex need its own knob?

---

## proposal: SuperGrok weekly-credits poller (unofficial endpoint — needs a risk decision)

**Problem.** shunt has zero xAI/Grok quota integration today. Official `api.x.ai` docs define
only per-model RPS/TPM limits with a `429` + backoff contract — no remaining%/reset signal
exists there. The actual SuperGrok weekly-credit-pool percent (the number a user would
recognize from Settings → Usage) has no official API at all.

**Evidence.** `docs/m6-xai-provider.md` (no quota signal documented); recon's xAI section:
the only endpoint reporting a weekly percent + reset is
`POST grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig`, an undocumented gRPC-web
protobuf call, reverse-engineered by third-party tooling (`steipete/CodexBar`,
`diegosouzapw/OmniRoute#6844`), authenticated primarily by **browser session cookies** — a
credential type distinct from the OAuth device-flow tokens shunt already holds for xAI
accounts (`~/.shunt/xai-auth.json`).

**Constraints.**
- Must not weaken or change how shunt authenticates its existing xAI OAuth accounts for
  inference traffic — this would be a wholly separate credential path, used only for the
  quota read.
- If pursued, the poller must fail closed to "no signal" (matches Codex being blank in
  today's `GET /usage`) rather than block or degrade inference traffic on a broken
  undocumented endpoint.

**Non-goals.**
- The official xAI Management API (prepaid balance / postpaid spend limits / spend
  analytics) — real, documented, but answers a different question (billing spend, not
  RPS/TPM or weekly-pool headroom) and would need its own ticket if an operator wants spend
  tracking rather than usage bars.
- Third-party-claimed `x-ratelimit-*` response headers on `api.x.ai` — unconfirmed against
  primary docs; not designed around a claim this recon could not verify.

**Open questions (for a maintainer, before any implementation):**
- Is depending on an undocumented protobuf contract with cookie-based auth acceptable for a
  gateway that otherwise only depends on documented, token-based provider APIs? This is a
  risk-tolerance decision, not an engineering one.
- Does xAI have (or plan) an official replacement worth waiting for instead?

---

## proposal: generic-provider quota tracking for Kimi-shaped `anthropic`-kind providers

**Problem.** Kimi is configured in shunt purely as a generic `kind = "anthropic"` provider —
there is no Kimi-specific code path anywhere, and shunt's quota machinery
(`note_quota`/`note_codex_quota`/the usage poller) is wired only into the built-in
Claude/Codex account pools. The Kimi Code Coding Plan has a real, documented-by-example
quota concept (a shared weekly membership cap plus finer sub-windows, matching the recon's
description of "a membership-cycle cap shared with the `/kimi` CLI that hard-freezes when
hit") that shunt currently has no way to see or act on.

**Evidence.** `GET {base}/usages` on the Kimi Code base URL (`Bearer sk-kimi-*`), documented
by the official open-source `kimi-cli` (`src/kimi_cli/ui/shell/usage.py`), corroborated by
this machine's own `~/.kimi-code/config.toml` base URL and by a third-party tracker
(`Golden0Voyager/kimi-code-usage`) hitting the same endpoint independently. No response
headers carry any equivalent signal (`src/accounts.rs`'s `note_quota` only parses
Anthropic-brand headers, and is wired only to the OAuth Claude pool, not to generic
`anthropic`-kind providers).

**Constraints.**
- Kimi is not a first-class shunt provider today (`ProviderKind`/`AuthMode` have no Kimi
  variant) — any solution has to work for a *generically configured* provider, not add
  Kimi-specific branching, per AGENTS.md's "prefer table-driven config additions over
  hardcoded provider logic."
- Must not assume the endpoint's response shape is stable — the third-party tracker already
  handles two different observed shapes and a `/usage` singular-path fallback; a shunt
  poller would need the same tolerance.

**Non-goals.**
- Wiring this into `GET /api/oauth/usage` (M-A) or Claude Code's native bars at all — Kimi
  is not Claude, and the CLI's own renderer allowlist would drop it either way (see M-B).
- Any Kimi-specific adapter work beyond quota polling — out of scope here.

**Open questions.**
- Does "quota tracking for an arbitrary generic-provider account" belong in shunt's core
  account-pool model at all, or is it too speculative for a provider shunt does not
  officially support? This architectural question should be answered before any
  endpoint-parsing detail is designed — this ticket is scoped to raising it, not resolving
  it.
- If yes: does the User-Agent Kimi's gateway reportedly deprioritizes non-allowlisted UAs on
  (per community reports of `429 engine overloaded`) apply to this endpoint too, and if so
  what UA string is safe to send from a gateway that is not `kimi-cli` itself?

---

## Not drafted: Gemini and Ollama

Per the recon verdicts and `DESIGN.md`'s M-C sections, neither is proposed as an issue:

- **Gemini/Antigravity** — quota tracking is not separable from the much larger, explicitly
  out-of-scope decision of whether shunt becomes a Gemini provider at all; the only concrete
  quota signal found (Antigravity's local Connect-RPC endpoint) requires a co-resident live
  IDE process and a `ps`-scraped CSRF token, a trust model shunt does not use anywhere else.
  Revisit only if/when Gemini support itself is proposed.
- **Ollama** — no quota signal exists that fits shunt's credential model (API key/OAuth
  token); the only reported signal is an HTML scrape of a web page gated by a browser
  session cookie, and shunt has no native Ollama adapter to attach it to regardless. This is
  a clean YAGNI cut, not deferred work.
