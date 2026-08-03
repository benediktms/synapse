---
name: synapse
description: Durable cross-session memory via the `syn` CLI. Recall before starting substantive work or when the user references a past decision ("like we discussed", "the usual way"); save corrections, stated preferences, and decisions the code will not record.
---

# Synapse memory

`syn` is a memory store shared by every harness and machine. It holds facts that
outlive a session and are **not** derivable from the repository.

## Recall

```
syn recall "<topic>"            # active workspace + the user's preferences
syn recall "<topic>" -n 5       # cap results (default 10)
syn recall "<topic>" --all-workspaces
```

Run it:

- before starting substantive work on a task, using the task topic as the query;
- whenever the user references a past decision — "like we discussed", "the usual
  way", "as before";
- before saving anything (see below).

Hits look like this:

```
[m_7f2a] (work · fresha/offers, 2026-07-14) Staging deploys for offers-service go
through ArgoCD app `offers-stg`, NOT the deploy Slack bot.
[m_31bc] (preference, 2026-06-02) Benedikt prefers deploy verification via a
Datadog dashboard link over log tailing.
(2 results, 140ms)
```

The bracketed parenthetical is the hit's origin: a workspace (optionally
`workspace · project`) or `preference`.

## Save

```
syn save "<fact>" --type user|feedback|project|reference [--tags a,b]
syn remember "<fact>"
```

Save when you learn something durable:

| What happened | Command |
| --- | --- |
| The user corrects you, or states how they want things done in this codebase | `syn save … --type feedback` |
| A fact about the user that this project needs | `syn save … --type user` |
| A decision is made that the code will not record — a convention, a rationale, a rejected option | `syn save … --type project` |
| A URL, dashboard, ticket, or runbook worth keeping | `syn save … --type reference` |
| A preference that applies **everywhere**, in every project and workspace | `syn remember "<fact>"` |

`syn remember` is the one for durable user preferences — tooling choices,
communication style, blanket rules like "never force-push shared branches". It
takes no scope: it is not tied to a workspace or a project, and it surfaces in
recall everywhere. Do not use `syn save` for those.

`syn save` infers its scope from the current git remote, so a fact saved in a
repo belongs to that project. Pass `--scope workspace` for a fact that holds
across the whole workspace but is not a global preference; pass
`--scope owner/repo` to file it against a different project.

**Recall before you save.** Search the topic first. If a memory already covers
it, `syn edit` that id instead of creating a near-duplicate.

Never save what the repo already records — file layout, function names, what a
test asserts, anything a reader would find by opening the code.

Write memories self-contained: absolute dates ("2026-08-02", not "yesterday"),
full names, enough context to be understood cold in six months by someone who
was not in this session.

## Correct, remove, browse

```
syn edit <id> "<corrected fact>"
syn forget <id>
syn pin <id>            # keep it in the session-start digest
syn unpin <id>
syn show <id>
syn list
syn move <id> --to <workspace>   # it was filed in the wrong workspace
syn move <id> --to-preference    # it should apply everywhere
```

`syn move` keeps the id, the content and the original creation date, so reach
for it rather than forget-and-re-save when a memory is merely in the wrong
place. Its source flags say where the memory *is* (`--workspace`, `--preference`),
its `--to` flags where it belongs. Moving into preferences widens a
project-scoped memory to the whole workspace, since it now applies everywhere.

These act on one store, so tell them which one — take it from the hit line:

- workspace hit like `(work · fresha/offers, …)` → `--workspace work`
- preference hit like `(preference, …)` → `--preference`

Omit the flag and the command uses the workspace resolved for the current
directory, which is right when you are acting on a hit from that same workspace.

## Session start

`syn context` prints the digest for the current project — pinned memories, the
user's preferences, recent project memories. Harnesses with a session-start hook
inject it automatically. If you have not seen a `## Memory (syn context)` block
this session, run `syn context` yourself before your first substantive step.

## When the server is down

Reads fail fast with a one-line error — carry on without memory, do not retry in
a loop. Saves queue locally and report `queued … not yet recallable`; they flush
on the next successful command. Say so plainly rather than claiming the memory
was stored. `syn list --pending` shows the queue.
