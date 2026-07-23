# Wave 2 — Google Auth Store (A1) & Request Translation (B1) · DONE (2026-07-22)

Branch: `feat/gemini-provider`

## Accomplished
1. **TICKET-A1 (Google Auth Store & Token Self-Refresh)**:
   - Implemented `GoogleAuthStore` in `src/auth/google/auth.rs` & `src/auth/google/mod.rs`.
   - Reads `~/.gemini/oauth_creds.json` (or `GEMINI_AUTH_FILE`), handles atomic write ENOENT races gracefully.
   - Refreshes tokens 5 minutes prior to expiry using single-flight mutex (`REFRESH_LOCK`) against Google's OAuth endpoint with public gemini-cli credentials.
   - Discovers & caches `cloudaicompanionProject` via `loadCodeAssist`.
   - Exported `Credential::GoogleOauth` in `src/auth/mod.rs` & `default_google_auth_path()`.
   - Wired `Credential::GoogleOauth` across response adapters.

2. **TICKET-B1 (Request Translation)**:
   - Implemented `translate_request` in `src/model/gemini_request.rs` & `src/model/mod.rs`.
   - Converts Anthropic `MessagesRequest` (system instructions, multi-turn messages, base64 images, tools, tool choice, thinking budget) to Gemini `generateContent` format.
   - Provided `wrap_code_assist_envelope` for Code Assist outer envelope wrapping.

## Verification
- Comprehensive unit tests added in `src/auth/google/auth.rs` and `src/model/gemini_request.rs`.
- `cargo test`, `cargo clippy -D warnings`, and `cargo fmt --check` passing clean across workspace.
