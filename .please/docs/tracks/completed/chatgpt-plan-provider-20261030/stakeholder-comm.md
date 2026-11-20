## Resolution report

Hello. The ChatGPT plan support you asked for is implemented and up for review.

**What was missing**: shunt could reach a ChatGPT subscription only by reusing the Codex CLI's login. OpenAI's "Sign in with ChatGPT" preview, the sanctioned way for open-source tools to use plan usage, was not supported.

**What we built**: a new `chatgpt-plan` provider. You sign in one or more named ChatGPT accounts from the command line, and shunt serves the model ids you map to it from your ChatGPT plan. It keeps the sign-ins fresh, spreads requests across your accounts, and moves on to the next account when one reaches its usage limit. Signing out revokes the session at OpenAI and keeps only what a later sign-in needs.

**How to try it**: run `shunt login chatgpt-plan --name <account>`, finish the sign-in in your browser (or paste the callback URL into the terminal), add a `[providers.chatgpt-plan]` entry with the model ids you want routed there, and run Claude Code through shunt. `shunt logout chatgpt-plan --name <account>` signs the account out. The "ChatGPT plan" guide in the docs walks through each step.

**Release status**: PR #871 is in review. The change ships in a release after it merges. One end-to-end check against a real ChatGPT account is still outstanding before then.

If anything behaves differently from what you expect, let us know. Thank you.
