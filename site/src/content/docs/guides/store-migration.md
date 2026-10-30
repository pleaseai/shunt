---
title: Migrating to the Store
description: Move pool quota, gateway refresh tokens, and spend limits from their JSON state_path snapshots into a SQLite or PostgreSQL store, and check that the one-time import picked up every snapshot.
---

Without a [`[store]`](/reference/configuration/#store-optional), shunt keeps pool quota, gateway refresh tokens, and spend limits in three JSON snapshots, one `state_path` per subsystem. Adding `[store]` replaces them with one transactional database. There is no separate migration command: the first `shunt run` with `[store]` imports each snapshot once, then serves that subsystem from the store.

The import runs once per subsystem and cannot be repeated. Get the paths right before that first boot, because a snapshot it does not find is never imported later.

## What gets imported

| Subsystem | Snapshot | Imported when |
| :-- | :-- | :-- |
| Pool quota | `[server.pool] state_path` (no default) | `[store]` is set |
| Gateway refresh tokens | `[server.gateway] state_path`, default `~/.shunt/gateway-sessions.json` | `[server.gateway]` is configured |
| Spend limits and audit trail | `[server.spend] state_path`, default `~/.shunt/gateway-spend.json` | `[server.spend]` is configured |

A subsystem that is not configured is skipped and left unrecorded, so if you enable `[server.gateway]` or `[server.spend]` later, its snapshot is imported on the first boot after that.

Some state stays outside the store: provider credential files, admin browser sessions, OIDC state, cooldowns, and the spend meter counters in `<stem>.counters.json`.

## 1. Find and back up the snapshots

List the `state_path` of each configured subsystem, including the defaults above, and confirm each file exists where the shunt process will look. The defaults resolve against the home directory of the user that runs shunt, which under systemd or in a container may not be yours. shunt reads it from `HOME`, falling back to `USERPROFILE`; when neither is set, the gateway and spend subsystems have no default snapshot, so the first boot records `NoSnapshot` and never imports one. In that case set each `state_path` explicitly.

Copy the files somewhere safe. shunt only reads them during the import and never writes them while a store is attached, but a copy is the only way to inspect the pre-migration state later.

## 2. Add `[store]`

Keep every `state_path` key as it is. With `[store]` set, they tell shunt where to import from.

```toml
[store]
backend = "sqlite"
url = "/var/lib/shunt/shunt.db"
```

For PostgreSQL, set `backend = "postgres"` and a `postgres://` URL; a non-loopback host needs `sslmode=verify-full`. The [`[store]` reference](/reference/configuration/#store-optional) lists every key.

## 3. Run `shunt check`

```bash
shunt check
```

```text
config ok
store ok: sqlite sqlite:/var/lib/shunt/shunt.db, file does not exist yet; it is created with mode 0600 at boot
```

`check` opens the store read-only and never migrates, imports, or creates the SQLite file, so you can run it before you stop the running instance. A config error or an unreachable store fails here instead of at boot.

## 4. Stop the running instance

Stop the current shunt gracefully. Without a store, refresh tokens and spend limits are written to their snapshots as they change, so those snapshots are already current. Pool quota is not: a store-less shunt saves it every 15 seconds and does not save it again at shutdown, so its snapshot can miss up to the last 15 seconds of quota changes. Quota is re-derived from upstream responses, so the cost is a short cold start for the affected accounts; to avoid even that, stop sending traffic and wait 15 seconds before stopping shunt. Anything a store-less process writes to a snapshot after the import never reaches the store, so do not start the new instance until the old one has exited.

## 5. Start shunt with the store

```bash
shunt run
```

Before it serves, shunt creates or migrates the database, imports each snapshot in one transaction per subsystem, and restores from the store. The startup log line `store opened; state is served from it` reports each subsystem's outcome:

| Outcome | Meaning |
| :-- | :-- |
| `Imported { rows: N }` | The snapshot was copied into the store |
| `NoSnapshot` | The path was unset or the file was absent; a record was written and **this subsystem will never import** |
| `AlreadyImported` | A previous boot already handled this subsystem |
| `Off` | The subsystem is not configured, so it is not store-backed |

A failed import stops the boot with `importing the <subsystem> state snapshot <path> into the store failed`, followed by a colon and the reason. Nothing was imported for that subsystem, unless the reason is a failed commit: then the import may have landed, and the next boot reports that subsystem as `AlreadyImported` instead of re-reading your corrected snapshot. Subsystems imported before it keep their rows either way. Act on the reason: for a malformed or unsupported-version snapshot, fix or remove the named file; for an unreadable file, fix its permissions or path; for a failed store transaction, check the store itself. Then start again. [Troubleshooting](/reference/troubleshooting/) covers the remaining store startup errors.

## 6. Check the import

- Next to each imported snapshot there is usually a `<state_path>.imported` file recording the backend, the time, and the snapshot's SHA-256. shunt writes it after the import commits; if it cannot (for example, the snapshot's directory is read-only), it logs the warning `imported the snapshot but could not write its .imported sidecar` and carries on. A missing file does not mean the import failed: the startup outcome and `import_record` are what count.
- The store has one `import_record` row per store-backed subsystem. For SQLite:

  ```bash
  sqlite3 /var/lib/shunt/shunt.db \
    'SELECT subsystem, source_path, row_count FROM import_record'
  ```

  A `NULL` `source_path` means no snapshot was found for that subsystem.
- Gateway clients keep working without signing in again, and `GET /v1/organizations/spend_limits`, sent with the `[server.admin]` credential in `x-shunt-admin-token` (the default admin header) or `x-api-key` ([endpoints](/reference/endpoints/)), lists the same spend limits as before.

## If a snapshot was missed

A `NoSnapshot` outcome you did not expect usually means the path was wrong on the first boot: a different home directory, or a file that was not there yet. Placing the file afterwards changes nothing, because the import is decided by the `import_record` row, not by whether the tables are empty.

On SQLite, if the gateway has not issued, refreshed, or revoked any token and no spend limit has changed since that first boot, stop shunt, delete the database file together with its `-wal` and `-shm` files, fix the path, and start again. The snapshots are unchanged, so every subsystem imports afresh. Pool quota saved since then is lost too, but it is re-derived from upstream responses. Do not reset after token or limit changes: re-importing the old snapshot would bring back refresh tokens revoked since then and drop the ones issued in their place.

## Going back

The import is a copy, not a sync, and shunt does not export store state back to JSON. Removing `[store]` starts each subsystem from its snapshot as it was at import time, which can bring back revoked sessions and undo spend-limit changes made since. While a snapshot is unchanged since its import, a store-less boot logs a warning saying so. See [Going back to no store](https://github.com/pleaseai/shunt/blob/main/docs/store.md#going-back-to-no-store) for details.
