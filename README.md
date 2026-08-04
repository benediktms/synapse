# Synapse

Centralised agent memory. One server process owns the SQLite files, the embedding model
and all business rules; the `syn` CLI is a thin client whose only local state is a config
file and an offline outbox.

## Running the server

```sh
export SYNAPSE_TOKEN="$(openssl rand -hex 24)"
mkdir -p data                          # the bind mount, owned by you
printf 'SYNAPSE_TOKEN=%s\nSYNAPSE_UID=%s\nSYNAPSE_GID=%s\n' \
  "$SYNAPSE_TOKEN" "$(id -u)" "$(id -g)" > .env
docker compose up -d --build
curl -s localhost:8737/health          # {"status":"ready"}
```

`.env` is what makes this stick: `docker compose` reads it in the project directory, so
every later command runs the container as you rather than as the image's uid 10001. It
holds the token, so keep it out of version control (`.gitignore` already does).

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
host during verification. The image runs as an unprivileged `synapse` user (uid 10001),
which cannot write a directory that belongs to you — hence `SYNAPSE_UID` / `SYNAPSE_GID`
in `.env`. The mount is declared with `create_host_path: false`, so a missing `./data`
fails the start outright instead of being created as root and leaving the server unable
to open its databases.

### Maintenance commands

Both are offline operations: stop the server first, since a single writer owns the
directory.

```sh
docker compose stop
docker run --rm -v "$PWD/data:/data" --user "$(id -u):$(id -g)" \
  synapse-server:local reembed --model bge-small-en-v1.5
docker run --rm -v "$PWD/data:/data" --user "$(id -u):$(id -g)" \
  synapse-server:local fts-rebuild
docker compose start
```

`reembed` records its target durably before touching anything and converts one workspace
per transaction. If it dies mid-run the marker survives, the server refuses to report
`ready`, and re-running it skips the databases already converted and finishes the rest.

## Routing

Every `save`, `recall`, `context`, and id-based command needs to resolve which workspace
to use and which scope within it. Absent an explicit `--workspace`/`--project` flag, both
come from one git-facts probe per invocation (`git rev-parse` plus the origin URL), keyed
on the outermost working tree.

Workspace resolution follows a fixed order — the first tier that matches wins:

1. `--workspace <name>` on the command line.
2. A path rule (`syn workspace map <path> <ws>`). The current-directory tier is tried
   first — *any* matching rule there wins, however broad, before the anchor tier (a linked
   worktree's main checkout) is even considered; the two tiers compete on which one
   matched, not on which rule is more specific. So a blanket rule like
   `syn workspace map ~/.traycer/worktrees personal` beats an exact rule mapping one repo's
   main checkout, for every worktree under it. Within a single tier, the longest matching
   prefix wins.
3. An org rule (`syn workspace map-org <org> <ws>`), matched against the repo's `owner`
   case-insensitively (GitHub/GitLab treat `FreshaEngineering` and `freshaengineering` as
   the same org) — but only once no path rule matched at all.
4. Inside a working tree while saving: fail closed with an error naming the config path,
   rather than guessing.
5. The machine's default workspace (`syn workspace use <name>`), or an error if none is set.

`syn workspace list` prints path rules and org rules in this same order, so precedence is
visible instead of folklore.

Scope is separate from workspace: it always comes from the *innermost* repo's `owner/repo`
slug (`git config remote.origin.url`), regardless of which tier resolved the workspace. A
repo with no usable origin — no remote, or one that doesn't parse to `owner/repo` — falls
back to the `workspace` scope, sharing one bucket with every other origin-less repo in that
workspace. Adding a remote is the fix; there is no per-directory synthetic identity to fall
back to instead, since anything path-derived would be orphaned by a later rename.

A worked example — the pair of rules the org-rule tier exists to replace:

```sh
# before: every Traycer worktree needs its own rule, and ~/code can only guess
syn workspace map ~/.traycer/worktrees/freshaengineering__app-b2c-api-gateway work
syn workspace map ~/code work

# after: one org rule catches every Fresha repo, wherever it's cloned or worktreed
syn workspace map-org freshaengineering work
```

Every command that infers its workspace or scope hard-errors when `git` is missing or
misbehaves, rather than silently defaulting or falling back to `workspace` scope. Naming
both halves explicitly is what keeps a command git-free: `--workspace` together with
`--project`/`--scope` for a save, `--workspace` or `--scope everywhere` for `syn export`
and `syn import`. A save that names `--workspace` but not `--scope` still needs `git`, because
the scope comes from the repo's origin. The one exception is
`syn context`: the installed session-start hook swallows any CLI failure, so a broken
`git` degrades to *no digest* rather than a broken session — silent, but safe, and the
first thing to check when a digest goes missing.

### Routing migration

**Enabling an org rule is a routing migration, not a config tweak.** Stored memories don't
move by themselves — they stay in whichever workspace's SQLite file they were originally
written to. Path rules still win over org rules, so adding an org rule only reroutes the
repos no path rule already covers; a repo with an existing path rule keeps resolving
exactly as before until that rule is removed.

The moment a repo's *resolved* workspace would actually change, its existing memories are
stuck on the old side of a split: recall from the new route looks like it lost history,
and new saves land beside neither. There is no schema migration to warn you, so this
procedure is the only safeguard.

Per affected repo:

1. Record its currently resolved workspace before changing anything — `syn workspace list`
   shows the path rule that covers it today.
2. Add the org rule with `syn workspace map-org`. Keep the existing path rule in place —
   it still wins, so nothing reroutes yet.
3. Find every memory that belongs to this repo but lives in the old workspace:
   `syn list --workspace <old>`. Move each one: `syn move <id> --to <new>`. Run this from
   inside the repo — the path rule you kept in step 2 still resolves the move's source
   workspace correctly, so no `--workspace` override is needed.
4. Verify the memories landed correctly with `syn recall <query> --workspace <new>` (an
   explicit `--workspace`, not a bare `syn recall` from inside the repo — the path rule is
   still in place at this point and still wins, so ambient recall keeps searching `<old>`
   until the rule is gone; that's the moment described above where it looks like the
   history is lost).
5. Only then delete the superseded path rule. There is no `syn workspace unmap` — remove
   the corresponding `[[workspace_rules]]` entry from the config file directly. Ambient
   `syn recall`/`syn save` from inside the repo now resolve `<new>`.

`syn move` exists for exactly this. There is no bulk "move every memory for this repo"
command yet — worth building if the per-id procedure above proves tedious in practice.

## Backup and restore

Dumps are a versioned logical format — ids, content, kind, scope, tags, pinned flag and
timestamps. Embeddings and the FTS index are derived, so they are not in the dump and
import re-embeds with the active model. That also makes a dump model-agnostic.

Back up each workspace plus the everywhere memories, one file each:

```sh
for ws in $(syn workspace list); do syn export --workspace "$ws" > "backup/$ws.json"; done
syn export --scope everywhere > backup/everywhere.json
```

Restore into an empty server:

```sh
# 1. bring up a server on the new or wiped data directory.
#    wipe the contents, not the directory itself — compose refuses to start
#    if ./data is missing, rather than letting Docker create it as root.
mkdir -p data
docker compose up -d
curl -s localhost:8737/health

# 2. recreate each workspace, then import its dump.
#    `import` refuses a non-empty target unless you pass --merge.
syn workspace create work
syn import --workspace work < backup/work.json
syn import --scope everywhere < backup/everywhere.json

# 3. confirm
syn recall "something you know is in there"
```

Notes that matter when restoring:

- Import preserves the dump's timestamps and pinned flags rather than restamping `now`.
- `--merge` is idempotent by id, so a partial import is safe to re-run. A dump whose id
  already exists with *different* content is a conflict (409) and aborts the import — fix
  the collision rather than forcing it.
- The everywhere memories (`syn save --scope everywhere`) are a separate dump. Forgetting
  `--scope everywhere` silently leaves them out of the backup.
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
