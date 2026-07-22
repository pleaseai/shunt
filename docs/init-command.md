# `shunt init` starter configuration

`shunt init` creates a starter `shunt.toml` in an existing directory. The command is intentionally narrow and offline: it writes that one file, without editing other files, installing dependencies, creating directories, or accessing the network.

## Content generation

The starter begins with validation and run instructions. With no `--upstream`, the rest is commented guidance, so loading the file preserves shunt's default passthrough behavior. Repeating `--upstream <preset>` emits ordered `[[upstreams]]` entries and credential comments derived from the config preset registry, followed by a commented `models.upstream_model` example for the first requested preset. Because declaring `[[upstreams]]` replaces the built-in provider set, a trailing `anthropic` passthrough upstream is appended automatically (unless you request `anthropic` yourself) so the generated file passes `shunt check` and unmapped models keep their Anthropic fallback. It never sets `server.default_provider`, so unmapped traffic is not silently diverted.

The accepted preset names are `anthropic`, `codex`, `openai`, `xai`, `grok`, `kimi`, and `cursor`. Unknown and duplicate values fail before the file is opened.

## Command semantics

```text
shunt init [--upstream <preset>]... [--root <path>] [--force]
```

`--root` defaults to the current directory. The directory must already exist. Before writing, the command checks `shunt.toml`, `shunt.yaml`, and `shunt.yml` in config-discovery priority order. If any exists, the command fails without `--force` and leaves stdout clean.

Without `--force`, the final open uses create-new semantics so a file created after the guard cannot be overwritten. With `--force`, `shunt.toml` is created or replaced; existing YAML variants remain untouched. Config discovery gives the new TOML file precedence over adjacent `shunt.yaml` and `shunt.yml` files.

On success, stdout contains only `Wrote <path>`. An interactive stderr also receives a validation/run hint; redirected stderr remains clean.

## Deliberate deviation from flue

This mirrors `flue init`'s existing-directory requirement and variant guard, but `--upstream` is optional rather than requiring a target. An empty shunt config is already a working gateway that passes unmapped models through to Anthropic with the client's credential, while flue needs `--target` to choose a deployment runtime.
