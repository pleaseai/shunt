# Smoke driver hardening — two review rounds (2026-09)

Two review rounds of the run-shunt skill's smoke driver (`.claude/skills/run-shunt/smoke.sh` + `test_smoke.sh`), captured for the record. The findings landed with PR #609: the checked-in driver now carries the hardened shape (bounded port guard, deadline'd readiness, `--locked` build, live-impostor detection, `test_smoke.sh` regression). This file keeps the reasoning behind those shapes.

## Round d4 verdict (partial → reopened)

No path found where the driver reports success while testing a process other than the freshly built `shunt` binary. Core hardening held: `BIN` artifact resolution (jq over `cargo build --message-format=json-render-diagnostics`, honoring `CARGO_TARGET_DIR` and a configured target triple), `jobs -pr` liveness, `lsof -a -p` pid-scoped listener lookup, mock port-file ordering after bind, and cleanup. Findings, all minors or nits:

1. `validate_port` fed an unbounded digit string to `[ ... -gt ]`, so a value above `2^63-1` passed the guard and failed later at `shunt check` with a misleading message. The queued fix: match `^[0-9]{1,5}$` first (`validate_port`).
2. Readiness broke on `listen()` before the warm-up awaits finished serving, and the health curl had no deadline — a stalled warm-up hung the driver. The queued fix: two bounded phases — a listener poll (50 × 0.1s) and a health loop with `curl --connect-timeout 1 --max-time 1` × 10 (the `CURL_DEADLINE` array); a control script (a child that binds and never accepts) verified the bounded-refusal shape.
3. The port-mismatch guard was unreachable with a single listener — kept as a regression canary (now exercised by the live-impostor test from PR #609).
4. The mock accepted any POST, so the forward assertion was weak — nit, left as-is.
5. The build omitted `--locked` while the repo gate requires it — the queued fix: `cargo build --locked`.
6. The bind-collision impostor's HTTP handlers were dead weight (the collision kills the child at bind before any request) — nit, kept: the shipped impostor serves every assertion the driver makes and `assert_impostor_answers` drives each on every run.

## Round final verdict (met, against the queued series)

Every claim below describes the PR #609 scripts as reviewed.

- Reject a mock-port impostor: the owned child writes its port file only after `HTTPServer` binds, so an impostor makes the child exit and `job_running` reports the exit.
- Reject a shunt-port impostor: `listener_port` returns only a listener owned by the spawned pid — the pre-PR-#609 shape could catch a live impostor on the requested port only at bind; the follow-up added the live-impostor regression.
- Bound every readiness wait: phase 1 (50 × 0.1s listener poll) fails with `shunt bound no port`; phase 2 (10 × deadline'd curls) fails with `shunt did not answer HEAD / in time` plus the log.
- Ephemeral-port capability: `SHUNT_PORT=0` / `MOCK_PORT=0` documented in the skill (the `SHUNT_PORT`/`MOCK_PORT` paragraph); every cleanup path reaps with `kill` + `wait`.
- Static quality: `bash -n` and ShellCheck (style severity) clean on both scripts.
- `test_smoke.sh` green; default-port `smoke.sh` run green (both taken from the follow-up round's runs; this file records, it does not re-run).

## Deletion trigger

Delete this file when the smoke driver is rewritten or the run-shunt skill is removed; nothing else cites it.
