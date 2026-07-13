# Memory index

- [shunt otel privacy-claim rot](shunt-otel-privacy-claim-rot.md) — `include_session_id` in `src/config.rs`/`src/telemetry.rs` was documented but never wired into `src/proxy.rs`; always grep a privacy/gating config field's name across the whole `src/` tree before trusting doc-comment claims about it.
- [shunt "verbatim" convention](shunt-verbatim-terminology-convention.md) — shunt uses "verbatim" strictly for byte-identical passthrough; PR #114 had one loose use of it for a re-shaped error envelope.
