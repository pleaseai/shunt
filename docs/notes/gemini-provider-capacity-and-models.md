# Gemini Provider: Capacity, Model Support & Auth Architecture

**Last Updated:** 2026-07-22

---

## 1. Native Gemini Provider in Shunt (Subscription OAuth)

- **Auth Source:** `~/.gemini/oauth_creds.json` (Google One AI Pro subscription OAuth).
- **Cost:** **$0.00 / Free with subscription** (No per-token billing).
- **Backend:** Google Code Assist (`cloudcode-pa.googleapis.com`).

### Supported Models & Status
| Model Slug | Display Name in `/model` | Status |
| :--- | :--- | :--- |
| `gemini-2.5-pro` | `[GEM ] Gemini-2.5-Pro` | 🟢 **Stable, Fast, Unlimited Capacity** |
| `gemini-2.5-flash` | `[GEM ] Gemini-2.5-Flash` | 🟢 **Stable, Fast, Unlimited Capacity** |
| `gemini-3.1-pro-preview` | `[GEM ] Gemini-3.1-Pro (preview)` | 🔴 Server capacity capped by Google (429 `RESOURCE_EXHAUSTED`) |
| `gemini-3-flash-preview` | `[GEM ] Gemini-3-Flash (preview)` | 🔴 Server capacity capped by Google (429 `RESOURCE_EXHAUSTED`) |

> **Note:** Google Code Assist does not accept standard API slugs like `gemini-3.5-flash` or `gemini-3.6-flash` (returns `404 Requested entity was not found`). The 3.x preview endpoints are subject to severe Google server-side load shedding.

### Where to submit feedback / complain to Google
- **Google Issue Tracker (Public):** [issuetracker.google.com/issues?q=componentid:1432658](https://issuetracker.google.com/)
- **Gemini CLI GitHub Issues:** [github.com/google-gemini/gemini-cli/issues](https://github.com/google-gemini/gemini-cli/issues)
- **Google Cloud Community Forum:** [googlecloudcommunity.com/gc/AI-ML/bd-p/cloud-ai](https://www.googlecloudcommunity.com/)

---

## 2. Antigravity (`agy`) Sub-agent Integration

- **Execution:** Runs `agy -p "..."` via local CLI (`~/.gemini/antigravity-cli/bin/agy`).
- **Use Case:** Heavy file generation, deep repository searches, background delegations.
- **Claude Skill:** `/gemini` skill or `yuting0624/antigravity-for-claude-code` plugin.

---

## 3. Google AI Studio API Key (`GEMINI_API_KEY`) Billing & Cost

- **Backend:** `generativelanguage.googleapis.com`
- **Subscription Usage:** Does **NOT** run through your Google One AI subscription (AI Studio is a separate developer platform).
- **Free Tier:**
  - **Cost:** **$0.00** (Free, no credit card required for free tier).
  - **Limits:** 15 Requests Per Minute (RPM), 1,000,000 Tokens Per Minute (TPM), 1,500 Requests Per Day (RPD).
  - **Models Available:** `gemini-2.5-pro`, `gemini-2.5-flash`, `gemini-2.0-flash`, `gemini-1.5-pro`.
- **Pay-As-You-Go Tier:** Only if explicitly enabled on GCP with a credit card (bills per 1M tokens).
