# Synapse

Centralised agent memory. One server process owns the SQLite files, the embedding model
and all business rules; the `syn` CLI is a thin client whose only local state is a config
file and an offline outbox.

## Running the server

```sh
export SYNAPSE_TOKEN="$(openssl rand -hex 24)"
docker compose up -d --build
curl -s localhost:8737/health          # {"status":"ready"}
```

The image bakes the embedding model (bge-small-en-v1.5) into its layers, so a container
starts with no network access to the Hugging Face hub and no API keys. Migrations run on
boot, per workspace database. `/health` only reports `ready` once migrations have run, the
model has loaded, and every workspace database agrees with the runtime model.

Point a client at it:

```sh
syn config set-url http://127.0.0.1:8737
syn config set-token "$SYNAPSE_TOKEN"
syn workspace create work
syn workspace map ~/code/work work    # saves under this tree resolve to `work`
```

### Server environment

| Variable | Default | Notes |
| --- | --- | --- |
| `SYNAPSE_TOKEN` | — | Required. Static bearer token; every client presents it. |
| `SYNAPSE_DATA_DIR` | `/data` in the image | One SQLite file per workspace lives here. |
| `SYNAPSE_BIND` | `0.0.0.0:8737` in the image | Inside a container the bind must be `0.0.0.0`; the compose file publishes it to `127.0.0.1` only, so host exposure stays a deliberate act. |
| `SYNAPSE_ALLOW_NONLOCAL` | `1` in the image | Required for any non-loopback bind. Set outside Docker only if you mean it. |
| `HF_HOME` / `FASTEMBED_CACHE_DIR` | `/opt/synapse/model` | Both point at the baked model cache. `HF_HOME` wins in fastembed's resolution; both are set so an upstream precedence change cannot trigger a download. |
| `RUST_LOG` | `info` | Request logs redact the token and all bodies. |

`docker-compose.yml` bind-mounts `./data`, which makes the databases inspectable from the
host during verification. The image runs as an unprivileged `synapse` user (uid 10001), so
set `SYNAPSE_UID` / `SYNAPSE_GID` to your own ids when `./data` belongs to you:

```sh
export SYNAPSE_UID=$(id -u) SYNAPSE_GID=$(id -g)
```

### Maintenance commands

Both are offline operations: stop the server first, since a single writer owns the
directory.

```sh
docker compose stop
docker run --rm -v "$PWD/data:/data" --user "$SYNAPSE_UID:$SYNAPSE_GID" \
  synapse-server:local reembed --model bge-small-en-v1.5
docker run --rm -v "$PWD/data:/data" --user "$SYNAPSE_UID:$SYNAPSE_GID" \
  synapse-server:local fts-rebuild
docker compose start
```

`reembed` records its target durably before touching anything and converts one workspace
per transaction. If it dies mid-run the marker survives, the server refuses to report
`ready`, and re-running it skips the databases already converted and finishes the rest.

## Backup and restore

Dumps are a versioned logical format — ids, content, kind, scope, tags, pinned flag and
timestamps. Embeddings and the FTS index are derived, so they are not in the dump and
import re-embeds with the active model. That also makes a dump model-agnostic.

Back up each workspace plus the preferences, one file each:

```sh
for ws in $(syn workspace list); do syn export --workspace "$ws" > "backup/$ws.json"; done
syn export --preference > backup/preferences.json
```

Restore into an empty server:

```sh
# 1. bring up a server on the (new or wiped) volume
docker compose up -d
curl -s localhost:8737/health

# 2. recreate each workspace, then import its dump.
#    `import` refuses a non-empty target unless you pass --merge.
syn workspace create work
syn import --workspace work < backup/work.json
syn import --preference < backup/preferences.json

# 3. confirm
syn recall "something you know is in there"
```

Notes that matter when restoring:

- Import preserves the dump's timestamps and pinned flags rather than restamping `now`.
- `--merge` is idempotent by id, so a partial import is safe to re-run. A dump whose id
  already exists with *different* content is a conflict (409) and aborts the import — fix
  the collision rather than forcing it.
- Preferences ("applies everywhere" memories, written with `syn remember`) are a separate
  dump. Forgetting `--preference` silently leaves them out of the backup.
- A queued save is not on the server, so it is in no dump. `syn export` drains this
  machine's outbox first and refuses to write anything while saves remain queued — a
  backup either includes them or fails. It cannot see *other* machines' outboxes, so run
  the export from each client that might be holding saves, or clear them there first.
- Dead-lettered saves were rejected outright and will never reach a dump. `syn export`
  names them on stderr; `syn list --pending` shows why, and reassign or discard resolves
  them.

## Verification drills

`scripts/verify.sh` exercises the seven end-to-end drills against the containerised
server. It needs `docker`, `jq`, `sqlite3`, `python3` and `curl`, builds `syn` if it is
missing, and resets the stack and the `./data` volume before each drill.

```sh
scripts/verify.sh          # all seven
scripts/verify.sh 3 6      # just the outbox and isolation drills
```

| # | Drill |
| --- | --- |
| 1 | Save, sever the connection before the reply arrives, retry — the idempotent `PUT` collapses it into one memory. |
| 2 | Two client configs: one saves, the other recalls. |
| 3 | Save with the server stopped — queued locally; the next read command flushes it. |
| 4 | Export, wipe the volume, import, recall — the procedure documented above. |
| 5 | Cold start on an `--internal` Docker network with no route to the hub, reaching ready `/health`. |
| 6 | Workspace isolation: the default recall path never crosses workspaces, `--all-workspaces` does and groups its results, preferences surface in both. |
| 7 | Kill a registry-wide reembed mid-run: the server refuses `ready`, and a restart resumes and completes it. |

Drill 1 uses `scripts/sever-proxy.py`, which forwards a request upstream and then drops the
connection before relaying the reply — the client never learns that the save landed.
