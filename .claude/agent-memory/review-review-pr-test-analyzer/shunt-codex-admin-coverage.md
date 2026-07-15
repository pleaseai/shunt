---
name: shunt-codex-admin-coverage
description: PR #144 Codex admin OAuth tests cover happy path/storage/list/pool/delete well; remaining branch gaps are state/account-id rejection, Codex-specific CSRF route wiring, and pending-kind isolation.
metadata:
  type: project
---

Reviewed PR #144 (`amondnet/codex-account-web`) against full changed source and
`tests/admin_surface.rs`.

Strong coverage: both callback formats (`code#state` and full redirect URL), Codex
CLI authorize parameters/form exchange, stored `auth.json` shape, unit-level Unix
`0600` permissions, token-free metadata serialization, missing refresh-token 502,
invalid name/no-pending 400, list/pool inclusion, and file deletion.

Remaining real gaps:
1. No HTTP test exercises Codex completion with mismatched/empty OAuth state/code,
   so the security boundary at `src/admin/codex.rs:139` and its no-exchange/no-write
   behavior can regress unnoticed.
2. Missing-account-id after an otherwise valid token exchange is not tested at the
   handler boundary (`src/admin/codex.rs:169`); it must 502 and avoid writing a file.
3. Existing browser CSRF tests call only Claude routes. All three new Codex mutating
   routes invoke `check_csrf`, but no test proves their route wiring cannot bypass
   cookie-session CSRF.
4. The new provider-kind isolation guard (`src/admin/codex.rs:132`) is untested;
   a Codex completion must reject a non-Codex pending entry rather than exchanging
   it under ChatGPT semantics. Namespaced keys make this defensive branch difficult
   to hit through normal HTTP today, so it is minor.

`parse_callback_value` has useful direct coverage for URL decoding, `code#state`,
and no delimiter. Additional URL/query degenerates mostly converge on the handler's
state/code validation; the meaningful gap is the handler rejection contract, not an
exhaustive parser input table.
