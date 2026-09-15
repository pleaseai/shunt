# OpenCode Zen: CLI credential storage

**Last Updated:** 2026-09-14

---

## Where the `opencode` CLI stores the Zen key

The `opencode` CLI login writes the OpenCode Zen API key to `~/.local/share/opencode/auth.json` (mode 600):

- entry under provider id `opencode`, shape `{"type": "api", "key": "<string>"}`
- plaintext, no encryption, no keyring
- the same file holds api creds for `deepseek`, `openrouter`, `zai-coding-plan`, `lmstudio` and oauth entries for `github-copilot`, `google`

The key value was never read; existence and type only.

## Evidence

- measured 2026-09-14 by session `shunt-6b`: file exists, entry type confirmed, key value not read.
- re-confirmed by `stat`: mode 600, owner uwuclxdy.

## Consequence

The opencode provider page's claim that `OPENCODE_API_KEY` can be "the key an `opencode` CLI login stored" is accurate; the page names the storage path.
