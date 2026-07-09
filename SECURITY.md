# Security Policy

`shunt` is an early-stage LLM gateway that sits in the request path and handles provider
credentials (API keys and reused ChatGPT OAuth tokens). Please treat security reports carefully.

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Report privately to **minsu.lee@passionfactory.ai** (이민수 / Minsu Lee, @amondnet), or via the
repository's **Security → Report a vulnerability** (GitHub private vulnerability reporting) if
enabled.

Please include:

- affected component / endpoint and version or commit,
- a description and reproduction steps,
- impact assessment (what an attacker gains),
- any suggested remediation.

We aim to acknowledge reports within a few business days.

## Scope notes

- shunt forwards credentials to upstream providers and may **reuse `~/.codex/auth.json`**
  (ChatGPT OAuth tokens). Reports about token handling, logging of secrets, or token file
  permissions are in scope.
- shunt does **not** strip Claude Code's attribution block or rewrite request bodies; reports
  about unexpected body/credential leakage are in scope.

Please do not run automated scanners against infrastructure you do not own.
