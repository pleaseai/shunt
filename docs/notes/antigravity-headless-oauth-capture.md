# Headless Antigravity OAuth capture — extracted from t3code PR #9348

> **Amended 2026-09-04 after testing against the real `agy` CLI.** Sections 1-3 below
> describe the **ACP server**, which is NOT what shunt spawns. What actually applies to
> shunt's CLI transport is in "Verified against `agy`" at the bottom — read that first.
> The env knobs in section 3 (`GEMINI_HOME`, `AGY_ACP_FORCE_FILE_STORAGE`) do not exist in
> the `agy` CLI, and its OAuth flow is not loopback-based.

Source: https://github.com/pingdotgg/t3code/pull/9348 (merged, `feat(providers): add Google
Antigravity via the official ACP agent`). Files read: `apps/server/src/provider/
antigravityAuthSupport.ts`, `antigravityCallback.ts`, `antigravityRelease.ts`.

Relevance to shunt: the mechanics below are what admin remote provisioning needs to sign an
Antigravity account in from a browser that is **not** on the gateway host, and what a
multi-account Antigravity pool needs to keep tokens from colliding. They apply to the agent
binary regardless of whether we keep spawning `agy` or move to the ACP server.

## 1. Two ways the agent surfaces its authorization URL

**a. Plain stdout line.** The agent prints exactly:

```
Open the following link to authenticate the ACP server: <url>
```

t3code matches on that byte prefix in the stdout stream and lets every other line through to
the normal transport. They cap a single line at 16 MiB and the URL itself at 16 KiB.

**b. `BROWSER` shim (the reliable path).** Rather than trusting the print, they set `BROWSER`
to a tiny helper that the agent's Python launcher execs *instead of* an OS browser. The helper
writes a marker plus the JSON-quoted URL to **stderr** and exits:

```js
process.stderr.on("error",()=>process.exit(0)).write(
  "__MARKER__"+JSON.stringify(process.argv[1])+"\n",
  ()=>process.exit(0))
```

Constraints they hit, all worth copying:

- Python splits `BROWSER` on the platform path separator **before** parsing quotes, so the
  command string must contain no `:` on unix / no `;` on Windows. They validate this and bail.
- `%s` is the URL placeholder; the interpreter path must not itself contain `%s`, `\r`, `\n`
  or `\0`.
- EPIPE must still exit **0**, otherwise Python falls back to opening a real OS browser after
  a cancelled sign-in.
- They run a **preflight**: spawn the helper against `https://example.invalid/...` and assert
  stdout is empty and stderr is exactly the marker line. Cheap guard against a broken shim
  silently launching a browser on the server.

## 2. Completing the flow from another device

The agent runs a one-shot loopback listener on `127.0.0.1`. When the operator's browser is on
a different machine, that redirect fails in their browser — they paste the failed URL back,
and the host replays it locally.

`validateAntigravityCallbackUrl` rejects unless **all** hold:

- length <= 16384; parses as a URL
- `protocol === "http:"`, `hostname === "127.0.0.1"`
- `origin` and `pathname` equal the pending redirect URI (i.e. port-pinned to this process)
- empty `username`, `password`, `hash`
- exactly one `state`, equal to the pending state
- exactly one `code` XOR exactly one `error`
- at most one `iss`, and if present `https://accounts.google.com`

Then `forwardAntigravityCallback` does a single plain GET to that loopback URL with
`agent: false` (no proxy, no redirect following, no response logging), 10 s timeout, and
treats 2xx as success.

Note the pending state is bound to the *running* process — a restart invalidates it.

## 3. Per-account profile isolation

Each provider instance gets a private profile directory, `<state>/providers/antigravity/
<sha256(instanceId)>` (hashed so case-sensitive IDs stay distinct on case-insensitive
filesystems), and the agent is launched with:

```
GEMINI_HOME=<profile>
AGY_ACP_FORCE_FILE_STORAGE=1      # file tokens, not the OS keychain — two accounts can coexist
BROWSER=<shim command>
PYTHONUNBUFFERED=1
ELECTRON_RUN_AS_NODE=1
```

`AGY_ACP_FORCE_FILE_STORAGE=1` is the key to multi-account: without it accounts fight over one
keychain entry. Shunt's adapter today uses whatever is in the ambient `~/.gemini`.

They also **strip** these ambient vars from the child env (case-insensitively, for Windows) so
the host's own Google config cannot leak in and silently change which account/billing is used:

`GEMINI_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_PROJECT`,
`GOOGLE_CLOUD_LOCATION`, `GOOGLE_CLOUD_QUOTA_PROJECT`, `GOOGLE_GENAI_USE_VERTEXAI`,
`GCLOUD_PROJECT`, `CLOUDSDK_CORE_PROJECT`, `AGY_ACP_CCPA_PROJECT`, `AGY_ACP_ENABLE_OAUTH`,
`GEMINI_HOME`, `AGY_ACP_FORCE_FILE_STORAGE`, `ANTIGRAVITY_HARNESS_PATH`, `BROWSER`,
`PYTHONUNBUFFERED`, `ELECTRON_RUN_AS_NODE`.

A `settings.json` in the profile carries `{"auth":{"type":<method>}}` plus an optional
`gcp.{project,location}` — never a credential. Naming the method means a native logout clears
only that method's tokens.

## 4. Official ACP server binary (separate, lower-priority track)

Antigravity is on the ACP registry as a versioned server, no CLI and no npm package:
`agy_acp_server_20260818_01_RC01`, downloaded from `dl.google.com/agy-extensions/releases/...`
per platform with a pinned SHA-256. Payload is `agy_acp_server.par` plus a
`localharness_external` binary. Archives are 315-543 MB; the executables unpack to 0.3-1.5 GB.

Two gotchas from their write-up: Google serves the archive **gzipped**, so a pinned
`content-length` check rejects every real download; and there is no Intel-Mac build.

Not evidence about the HTTP path — this is still spawn-based, which is consistent with our
finding that the direct HTTP provider is quota-blocked (see `antigravity-http-quota-blocked`).

## What is *not* worth taking

Their ACP session adapter, permission-mode mapping, mobile setup UI, and bundled model
manifest. Shunt speaks Anthropic Messages over HTTP and has no agent-session surface.


## Verified against `agy` (2026-09-04) — what actually applies to shunt

Test: `HOME=$(mktemp -d) BROWSER=/usr/bin/true agy -p "..." --model gemini-3-flash`.
Independently reproduced by two advisor models (GPT-6 Astra used the cheaper
`agy models` probe, which reports "Please sign in to view available models").

**1. `HOME` alone isolates an account.** `agy` resolves its entire state tree through
`$HOME` — not `getpwuid`, not a hardcoded path. A fresh `HOME` made it rebuild
`$HOME/.gemini/{config,antigravity-cli}/` plus `$HOME/Library/Caches` and demand its own
sign-in. So per-account isolation needs no ACP server, no 500 MB download: point `HOME` at
a private directory per provider entry. This is what shunt's `profile_dir` key does.

**2. It does not fall back to the OS keychain.** The `agy` binary does contain keychain
symbols, but if the token were keychain-backed the fresh-`HOME` run would have answered
instead of demanding login. File storage is the default, corroborated by the live token
being a plain 600-mode file at `~/.gemini/antigravity-cli/antigravity-oauth-token`.
t3code's `AGY_ACP_FORCE_FILE_STORAGE` solves a problem the CLI does not have.

*Still unverified:* that two independently authenticated profiles keep distinct identities
across token refresh and concurrent use. That needs a real second Google account.

**3. Headless provisioning is simpler than t3code's.** The CLI's redirect URI is
`https://antigravity.google/oauth-callback` — a hosted callback, not a `127.0.0.1`
listener. It prints the URL on stdout under `Authentication required. Please visit the URL
to log in:` and then offers, on stdin:

```
Or, paste the authorization code here and press Enter:
```

So remote provisioning is: capture the URL from stdout → operator signs in from any
browser → feed the authorization code back on stdin (60 s window; re-run if missed). None
of the `BROWSER` shim, the Python `BROWSER`-parsing traps, the loopback port pinning, or
the 8-check callback validation in sections 1-2 is needed for this path.

**4. Worth keeping from t3code regardless: env hygiene.** Strip ambient `GOOGLE_*` /
`GEMINI_*` from the child environment so the gateway host's own config cannot silently
change which account — and whose billing — serves a request.

## Caveat on where this landed

First implemented on `feat/antigravity-headless-oauth`, branched off `main` (9b87e83,
v0.24.0). That was the wrong line: `main` predates commit 2708d5e,
`feat(antigravity)!: reach Antigravity over HTTP instead of the local agy CLI`, which
demotes the CLI to the deprecated `antigravity_cli` transport and is what the running
gateway is built from.

The `profile_dir` key therefore lives on `feat/antigravity-profile-dir-040` (commit
63fecfd), branched off `build/0.40.1-astra-local`. The 0.24 attempt was reverted.
