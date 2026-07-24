# STATUS — Native Gemini provider (Path B)

Last updated: 2026-07-25 (issue #237 Gemini 3 tool continuation fix)

## Stage
Provider implementation is open in #234. Issue #237 reproduced a Gemini 3 tool-continuation
failure: response translation dropped `thoughtSignature`, so reconstructed `functionCall`
history received HTTP 400. The fix is in progress on `fix/237-gemini-thought-signatures`:
carry exact signatures in namespaced Anthropic `tool_use.id` values and restore them on the
next Gemini request; use Google's documented placeholder only for imported unsigned Gemini 3
history. Focused translation tests pass; full quality gates pass. A live Gemini 3.1 Pro response
proved the provider returns a signature on its tool call, but provider 429s blocked the immediate
continuation probe; retry after quota cooldown remains.

**AUTH DECISION (2026-07-21, amended 2026-07-22): reuse the gemini-cli subscription token.**
Hard constraint from user: must use the Google One AI Pro subscription at ZERO added cost.
S1 proved that refresh with the Gemini CLI installed-app credentials works end-to-end
(→ gemini-3-flash-preview returned SHUNT-OK). Those credentials are not committed:
shunt uses an unexpired access token directly, while shunt-side refresh requires
`SHUNT_GOOGLE_CLIENT_ID` and `SHUNT_GOOGLE_CLIENT_SECRET`; rerunning `gemini login` is the
fallback. Antigravity token can't be refreshed by us (obfuscated secret / Path C); AI Studio
API key rejected (adds per-token cost).
- **A1 rewritten** to: read `~/.gemini/oauth_creds.json` (tolerate atomic-rewrite ENOENT),
  optionally refresh with operator-supplied client credentials, and discover the
  loadCodeAssist project.
- **A2 DROPPED** (own-PKCE login is dead — Google refuses new logins on the gemini-cli client).
- **Known fragility:** Google is sunsetting this client for individuals. Rerun the Gemini CLI
  login if the shared token expires and no refresh client credentials are configured.

External root-cause review and final-diff review were run with Codex for #237. Codex confirmed
the original signature-loss path, caught an intermediate carrier-association gap that was removed,
and found no surviving defect in the final `tool_use.id` design.

## Base state (clean before tickets)
- Repo: `shunt`. Currently on `feat/214-admin-live-activity` (UNRELATED). **Create `feat/NN-gemini-provider` off a clean `main`** + a tracking issue before TICKET-C1/A1/etc. (S1 is a scratch spike, no branch needed).
- shunt today: 3 adapter kinds (anthropic/responses/cursor); no Gemini, no Google auth. This plan flips `docs/comparison.md` item I ("native Gemini not in scope").
- Verified slugs on the account's standard-tier: `gemini-3.1-pro-preview`, `gemini-3-flash-preview` (also `gemini-2.5-pro`/`-flash`). `gemini-3.1-pro`/`3.6-flash`/`3.5-flash` all 404.

## Ticket board
| Ticket | Stream | Status | depends-on | Wave |
|--------|--------|--------|------------|------|
| S1 | De-risk (auth spike) | DONE | — | 1 |
| C1 | Config foundation | DONE | — | 1 |
| A1 | Google auth module | DONE | S1, C1 | 2 |
| B1 | Request translation | DONE | C1 | 2 |
| B2 | Response/SSE translation | DONE | B1 | 3 |
| A2 | Own-PKCE login (v2) | DROPPED | A1 | 3 |
| D1 | Adapter + dispatch | DONE | A1, B2, C1 | 4 |
| T1 | Integration tests | DONE | D1 | 5 |
| X1 | Docs (all surfaces) | DONE | D1 | 5 |
| G1 | Agent layer (the goal) | DONE | D1 | 5 |
| E1 | Multi-account pool (v2) | TODO | D1 | 5 |

## Dispatch waves
- Wave 1: **S1** (spike, gates auth) ∥ **C1** (config, gates everything) — disjoint.
- Wave 2: **A1** (auth) ∥ **B1** (request xlate) — disjoint files.
- Wave 3: **B2** (stream xlate).
- Wave 4: **D1** (adapter + dispatch).
- Wave 5: **T1** ∥ **X1** ∥ **G1** ∥ **E1** (v2) — disjoint files.

Critical path to first spawnable Gemini agent: S1 → C1 → A1/B1 → B2 → D1 → G1.
A2 (own login) was dropped; E1 (multi-account concurrency) is the remaining v2 hardening.

## Next action
Retry the live two-tool Gemini 3 continuation after the provider quota cooldown. The deterministic
round-trip suite, format, clippy, and all 1,189 workspace tests pass; the final diff's independent
review found no surviving defect. After the live retry, update #237/#234 and commit the focused
fix once the user approves the commit plan.
