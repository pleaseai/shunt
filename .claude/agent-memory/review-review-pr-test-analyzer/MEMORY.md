# Memory index

- [shunt multi-account failover coverage](shunt-multi-account-failover-coverage.md) — PR #70 tests/multi_account.rs+accounts.rs+claude_store.rs gap analysis; failover matrix fully covered but 5 real gaps (PauseSame-success, setup_token detection, negative validation, refresh_lock concurrency, account_uuid wiring) + a test-naming-vs-assertion consistency pattern to check on future PRs.
- [shunt admin surface coverage](shunt-admin-surface-coverage.md) — PR #85 tests/admin_surface.rs gap analysis; happy path solid but session+CSRF success path, logout, OAuth state-mismatch, and escape_html untested; recurring "kind"-branch-coverage pattern.
