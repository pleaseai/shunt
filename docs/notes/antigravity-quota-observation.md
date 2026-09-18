# Antigravity quota observation — why the usage surface has no Google row

**Date:** 2026-09-06, scope corrected 2026-09-18
**Status:** the Google-API route is blocked; the local route is not (see
"Correction" below)
**Verified against:** `agy` (Antigravity CLI) on macOS, two live Google accounts,
shunt 0.41.0. Re-checked against `main` @ 6e7ccb1.

> **Correction (2026-09-18).** This note originally recommended leaving
> Antigravity unobserved. That conclusion was too broad: it rules out *Google's
> quota APIs*, which is correct and is what the probes below establish, but shunt
> does not need them. `src/auth/observation.rs` already reads Antigravity quota
> locally over the IDE language server's loopback RPC
> (`/exa.language_server_pb.LanguageServerService/GetUserStatus`, via
> `discover_antigravity_connection` + `fetch_antigravity_quota_from`), with no
> Google credential involved. That path is sound and simply never matches: its
> process-table substring is the old install location, which is #308. Read this
> note as "do not spend time on the Google APIs", not as "Antigravity cannot be
> observed".

## The question

`shunt-usage` / the admin surface shows a `GEMINI` row that reads
`~/.gemini/oauth_creds.json` — the **Gemini Code Assist** credential — and
reports `Needs login`, because individual Code Assist accounts are sunset.

Meanwhile every Gemini route in production goes through the `antigravity_cli`
provider, which drives the `agy` binary against a *different* credential:
`~/.gemini/antigravity-cli/antigravity-oauth-token`. Same Google identity, two
tokens, two APIs.

So the visible row reports on the credential that is **not** serving traffic,
and no row exists for the credential that is. With `profile_dir` (see
`src/config.rs`) several Antigravity accounts can now be pooled, which makes the
gap worse: none of them are observable.

The question was whether shunt could read Antigravity's own quota and render one
row per `profile_dir`.

## Answer: no, on two independent grounds

### 1. shunt cannot authenticate as the Antigravity client

- The stored token file is `{"auth_method", "token": {access_token, expiry,
  refresh_token, token_type}}`.
- The **primary** profile's `access_token` expired 2026-08-31 and the file has
  not been rewritten since (`stat` mtime unchanged), yet `agy` keeps serving
  requests. It refreshes in memory and does not write back. A reader of the file
  therefore gets an expired token most of the time.
- Refreshing independently fails. Two OAuth client ids are embedded in the
  binary (`1071006060591-…` and `884354919052-…`). Both are **confidential**
  clients: a refresh without `client_secret` is rejected with
  `invalid_request: client_secret is missing`. Exactly one `GOCSPX-` secret
  appears in the binary and it matches neither
  (`invalid_client: The provided client secret is invalid`).

A usage reader that can neither trust the stored token nor mint a new one has no
starting point.

### 2. The reachable quota endpoint reports the wrong product

Endpoints probed with a **freshly refreshed** token (forced by running a real
`agy` turn, which does rewrite the token in a `profile_dir` profile):

| Endpoint | Result |
| --- | --- |
| `aicode.googleapis.com` gRPC `/google.internal.cloud.code.v1internal.PredictionService/RetrieveUserQuotaSummary` | `404` — not exposed there |
| `aicode.googleapis.com/v1internal:retrieveUserQuotaSummary` (JSON) | `404` — gRPC-only host, no REST binding |
| `daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` | `200`, but carries only tier eligibility. No quota, credit, limit, or reset fields. |
| `daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary` | **`403 SUBSCRIPTION_REQUIRED`** (`domain: cloudaicompanion.googleapis.com`) |
| `…:retrieveUserQuota` | `403`, same |
| `…:fetchQuotaStatus`, `…:getAvailableCredits` | `404` |

The two methods that exist are the **Code Assist** quota — the same licensing
path that already makes the `GEMINI` row say `Needs login`. They are not
Antigravity's quota. An account that `agy` serves happily is refused here.

`agy`'s own log confirms the split: `quota_manager.go:45 doRefreshQuota` fires
with no corresponding `http_helpers.go:296 URL:` line, so the real quota call
does not go through the logged HTTP path at all.

## What exists but cannot be reached

`agy` feeds its **statusline hook** a JSON payload containing
`quota["gemini-5h"].remaining_fraction` and `quota["gemini-5h"].reset_in_seconds`
— the exact numbers wanted, already parsed, no protobuf involved. The user's own
`~/.gemini/antigravity-cli/statusline.sh` reads them.

**Tested and refuted:** the statusline does not fire in print mode. A
`statusLine` command configured in a `profile_dir` profile's `settings.json`
produced no payload across an `agy -p …` turn, and print mode is the only mode
shunt uses. (The probe was removed afterwards; the profile's `settings.json` is
back to empty.)

`agy --output-format json` returns `conversation_id`, `duration_seconds`,
`num_turns`, `response`, `status`, `usage` — token counts only, no quota.
`agy --help` exposes no `usage`/`quota` subcommand.

## Remaining lead, not pursued

`agy remote-control` runs a background daemon (`start`, `status`, `stop`). If it
hosts a session closer to interactive than print mode, the statusline hook may
fire there, which would make the quota harvestable without any Google API call.
Unknown; a separate investigation.

## Recommendation

Do not spend further effort on Google's quota APIs. Both grounds above are
authentication and product-scope problems, not schema problems, and no amount of
descriptor extraction moves them.

The observable path is already in the tree and needs no Google credential:
`discover_antigravity_connection` locates the running language server and
`fetch_antigravity_quota_from` reads `GetUserStatus` over its loopback RPC. It
reports nothing today only because the process-table match is pinned to
`/Applications/Antigravity.app/Contents/Resources/bin/language_server`, a path
the shipping app no longer installs to (#308). Fixing that match is the whole
job; everything downstream of it already exists.

Caveat on that fix: it should be made with Antigravity actually running, so the
new match is checked against a live process table rather than against a path
copied out of an issue. It was not running when this correction was written, so
the fix is left to #308 rather than guessed at here.

The `agy remote-control` lead below remains the fallback if the language-server
route is ever removed, but it is no longer the cheapest next probe.

Do **not** re-derive this by extracting protobuf descriptors from the binary:
the descriptors are present (`RetrieveUserQuotaSummaryRequest/Response`,
`GetG1Credits`, `credits.proto`) and are not the obstacle.
