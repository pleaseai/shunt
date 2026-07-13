# Memory index

- [shunt multi-account failover coverage](shunt-multi-account-failover-coverage.md) — PR #70 tests/multi_account.rs+accounts.rs+claude_store.rs gap analysis; failover matrix fully covered but 5 real gaps (PauseSame-success, setup_token detection, negative validation, refresh_lock concurrency, account_uuid wiring) + a test-naming-vs-assertion consistency pattern to check on future PRs.
