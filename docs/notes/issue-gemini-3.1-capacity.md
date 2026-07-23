# Issue: Gemini 3.1 Pro & 3-Flash Capacity Constraints on Code Assist REST vs Antigravity gRPC

## Overview
When routing requests through `shunt` using subscription OAuth (`~/.gemini/oauth_creds.json`) on the Google Code Assist REST endpoint (`cloudcode-pa.googleapis.com/v1internal:streamGenerateContent`), `gemini-3.1-pro-preview` and `gemini-3-flash-preview` frequently return HTTP 429:

```json
{
  "error": {
    "code": 429,
    "message": "No capacity available for model gemini-3.1-pro-preview on the server",
    "status": "RESOURCE_EXHAUSTED"
  }
}
```

## Empirical Probe Findings

1. **REST Code Assist (`cloudcode-pa.googleapis.com`)**:
   - `gemini-2.5-pro` & `gemini-2.5-flash`: **HTTP 200 OK** (Instant response, full capacity).
   - `gemini-3.1-pro-preview`: **HTTP 429 Resource Exhausted** (Server allocation capped by Google).

2. **Antigravity CLI / gRPC (`agy`)**:
   - Running `agy -p "..." --model gemini-3.1-pro --effort low` connects to Google's Antigravity gRPC infrastructure (`antigravity-unleash.goog`).
   - **Result:** Returns **200 OK** (`AGY-3.1-PRO-OK`) with high capacity and reasoning effort support!

## Conclusion & Workarounds

1. **For 3.1 Pro / 3-Flash Tasks**: Use `agy` (Antigravity CLI) via the `/gemini` skill or subagent delegation (`agy -p ... --model gemini-3.1-pro --effort low`).
2. **For Native Claude Code `/model` Switching**: Use `[GEM ] Gemini-2.5-Pro` or `[GEM ] Gemini-2.5-Flash` in `shunt` for instant execution without rate-limiting.
