# Synapse

Agent memory, replicated. A per-machine daemon (`synd`) owns the embedding model and all
business rules; the `syn` CLI is a thin client whose only local state is a config file and
an offline outbox. Turso holds the memories, one database per workspace.

## Local daemon durability

The local daemon treats Turso as the source of truth. Its embedded replicas are read
caches: reads run locally, while every mutation is sent to the primary and returns
success only after the primary commits it. `syn sync` only pulls committed primary
frames into those caches.

Offline saves remain durable without weakening that guarantee. Before contacting the
daemon, `syn save` writes the complete request to the CLI outbox. A transport or primary
failure leaves it queued and reports that it is not yet recallable; the next read command
tries to flush it. Inspect unresolved items with `syn list --pending`.

When upgrading a machine from a local-write replica build, first stop its daemon and
archive every replica file and sidecar before running any `syn` read or export command.
Confirm that the primary contains every acknowledged memory, then remove the old replica
files and let the remote-first daemon pull fresh caches. This avoids treating a stale
local WAL as authoritative during the cutover.

## Installing

```sh
just install          # builds, symlinks syn + synd into ~/.local/bin, loads the daemon unit
syn setup             # Turso orgs the daemon replicates, plus this machine's routing
```

`syn setup` is the whole first-run flow: it stores an org-scoped Turso token per org,
adopts every workspace database in those orgs, picks this machine's default workspace, and
writes the routing rules. Nothing else needs configuring — there is no URL and no bearer
token, because the daemon's boundary is a `0600` unix socket rather than a TCP port.

The daemon binary embeds the embedding model (bge-small-en-v1.5), so it starts with no
network access to the Hugging Face hub and no API keys. `syn status` reports each replica's
freshness and whether the last sync reached the primary.

`syn` starts the daemon on demand if nothing answers the socket. Set
`SYNAPSE_NO_DAEMON_AUTOSTART=1` to suppress that — useful when launchd or systemd owns the
process, and required by the test suite.

### Maintenance

```sh
synd reembed --model <name>    # walk every replica onto another embedding model
syn daemon logs -f             # tail the daemon's log
syn daemon restart
```

`reembed` records its target durably before touching anything and converts one workspace
at a time. If it dies mid-run the marker survives, the daemon refuses to report `ready`,
and re-running it finishes the rest.

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

Restore into an empty org:

```sh
# 1. point the daemon at the empty org and confirm it is serving.
syn setup
syn status

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
- A queued save has not reached the primary, so it is in no dump. `syn export` drains this
  machine's outbox first and refuses to write anything while saves remain queued — a
  backup either includes them or fails. It cannot see *other* machines' outboxes, so run
  the export from each client that might be holding saves, or clear them there first.
- Dead-lettered saves were rejected outright and will never reach a dump. `syn export`
  names them on stderr; `syn list --pending` shows why, and reassign or discard resolves
  them.
