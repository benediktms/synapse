---
name: synapse
description: Durable cross-session synapses via the `syn` CLI — the store for anything that should outlive this session. Read it before answering from assumption, whenever the user names a program, service, convention, or decision you cannot account for from the repo in front of you, or points backwards ("like we discussed", "the usual way", "as before"), or before starting substantive work on a subsystem. Write to it, unprompted, the moment a decision is reached, a preference is stated, or you are corrected — durable facts only, never tasks or reminders. This is the store to reach for even if the harness also offers a memory directory of markdown files — synapses carry a scope that such files cannot express, so use `syn` and do not mirror the same fact into both. Use it even when a session-start digest has already appeared: the digest is a few lines out of the whole store, not a substitute for querying it.
---

# Synapse protocol

`syn` holds **synapses**: durable facts about the user and their work, shared by
every harness, machine, and session, and not derivable from the repository. The
CLI prints the word "memory" and the commands keep it — but think in synapses,
because the harness's own memory system is a different thing (step 0).

Four steps, in order: **read → pick the move → pick the scope → write**. The order
matters in one place only, and it is load-bearing: reading is what tells you which
move and which scope, so a write that skipped step 1 is a guess.

## 0. Synapses are not the harness's memory files

Most harnesses ship a memory system — usually a directory of markdown files with
an index. If this one has one, it is neither a substitute nor a mirror.

A synapse carries a **scope**; a file in a memory directory cannot express one. It
also does not depend on where the harness was launched: instruction files and
memory directories are read only when a session starts somewhere that loads them,
while a synapse is reachable from any directory inside its scope. Write the
synapse and stop there. Mirroring a fact into both stores leaves two copies that
drift, and the scopeless copy is the one that goes wrong.

## 1. Read

```
syn recall "<topic>"            # active workspace + everything global
syn recall "<topic>" -n 5       # cap results (default 10)
syn recall "<topic>" --all-workspaces
```

**The digest is not the store.** A session-start hook injects `syn context` —
four or five lines, pinned and recent, chosen before anyone knew what this session
would be about. It tells you synapses exist; it does not tell you what they say.
Having seen the digest is the most common reason recall gets skipped, and it is
not a reason.

Recall when:

- **A name you cannot account for** — the user mentions a program, service, tool,
  or convention you cannot explain from this repo or this session. Recall before
  guessing, and before asking them to re-explain something they may have told you
  once already. An unfamiliar proper noun is the strongest signal there is.
- **Before substantive work on a subsystem** — query the topic. Rejected
  alternatives, why a thing is shaped oddly, a convention that outlived its
  author: none of that is in the code.
- **The user points backwards** — "like we discussed", "the usual way", "as
  before", "you know how I like it".
- **Before any write** — see step 2.

Skip it when the question is fully answered by code in front of you. Recall on
every trivial turn is noise, and noise is what gets the habit dropped.

Hits name their origin — a workspace, `workspace · project`, or `preference`:

```
[m_7f2a] (work · fresha/offers, 2026-07-14) Staging deploys for offers-service go
through ArgoCD app `offers-stg`, NOT the deploy Slack bot.
[m_31bc] (preference, 2026-06-02) Benedikt prefers deploy verification via a
Datadog dashboard link over log tailing.
(2 results, 140ms)
```

## 2. Pick the move

A store that only grows becomes noise, and once the user stops reading the digest
you have lost the channel, not just one fact. So recording is tending a network,
not appending to a log. Four moves:

| Move | Command | When |
| --- | --- | --- |
| **Grow** | `syn save` / `syn remember` | nothing covers this yet |
| **Strengthen** | `syn edit <id>` | a synapse is nearly right and this sharpens it — two half-true synapses on one topic are worse than either alone |
| **Prune** | `syn forget <id>` | a decision made an old synapse false; a retirement invalidates synapses, it does not merely add one |
| **Rewire** | `syn move <id>` | true, but filed where the sessions that need it will never look |

Recall the topic first. Not as a rule to obey — it is the only way to know which
of the four you are in.

## 3. Pick the scope

Scope is what a plain memory file cannot express, and getting it wrong is the most
common way a store goes bad. Decide it **per fact**: two facts in one turn often
belong in two different places.

The question that separates them: **would this still be true, and still worth
knowing, in a codebase that has nothing to do with this one?**

- **Yes → `syn remember`.** Facts about the user and how they want to be worked
  with: communication style, tooling choices, blanket rules like "never
  force-push shared branches". No scope flag, because it has no scope.
- **No, it concerns this body of work → `syn save … --scope workspace`.** It names
  the user's own programs, repos or conventions and spans more than one of them. A
  decision about how two of their projects relate belongs to neither alone, and
  filing it under one hides it from the other. Also correct when the checkout has
  no git remote, since there is nothing to infer from.
- **No, and it is one repo's business → `syn save …`.** Scope comes from the git
  remote. `--scope owner/repo` files it against a different project than the one
  you are standing in.

A useful tell: a fact that names a program is almost never a `syn remember`.
Project architecture is not a preference.

**When torn, go narrower.** The mistakes are not symmetrical. An over-scoped
synapse is merely hidden and `--all-workspaces` still finds it; an over-globalised
one surfaces in every unrelated session forever. A workspace holds one coherent
slice of the user's life — their own projects, or their employer's — and a fact
from one slice is usually wrong in the other.

## 4. Write

```
syn save "<fact>" --type user|feedback|project|reference [--scope …] [--tags a,b]
syn remember "<fact>"
```

`--type` records what kind of fact it is: `feedback` for a correction or a stated
way of working, `user` for something about the person, `project` for a decision
the code will not record, `reference` for a URL, dashboard, ticket or runbook.

Write in the same turn it happens. A decision is durable the moment it is reached;
deferring to a tidier moment is how it gets lost. Do not ask permission — a write
is cheap and `syn forget` undoes it, whereas a decision never written costs the
user the whole argument again in six weeks.

**Report each write in one line**, after your answer, so a wrong or badly-scoped
synapse gets caught while it is still cheap: `saved [m_00B1] brain retired →
repo-link + synapse`. Not a section, not a justification.

### What earns a place

- **Not what the code says** — file layout, function names, what a test asserts.
  A synapse earns its place by surviving the code changing around it.
- **Not a task or a reminder.** "Remember to add the flag later" is work to be
  tracked, not a fact to be known — it goes stale the moment it is done. This is
  not a routing problem to solve by writing it somewhere else instead: a to-do
  does not belong in a synapse *or* in a memory file. Say in your reply that it is
  deferred and leave it to whatever tracks the user's work. What is worth keeping
  is the *decision* behind a deferral, if there was one — "we chose not to do X
  because Y" is a durable fact; "do X later" is not.
- **Yes to instructions, deliberately.** A rule in `CLAUDE.md`, `AGENTS.md`, a
  contributing guide, a skill or an MCP server's notes reaches only sessions that
  load that file — which depends on the harness and on where it was invoked. Being
  written down in *one* place is the reason to store it, not a reason to skip it.
  Scope it by how far it must reach: a global instruction file, or anything about
  how the user wants to be worked with, is `syn remember`; one repo's convention
  takes project scope, or workspace scope if someone could need it while standing
  elsewhere.

Write them self-contained: absolute dates ("2026-08-03", not "yesterday"), full
names, enough context to be read cold in six months by someone who was not here.
A synapse is read by a stranger and cannot lean on anything that was only true in
the conversation that produced it.

## Reference

```
syn edit <id> "<corrected fact>"
syn forget <id>
syn move <id> --to <workspace>   # filed in the wrong workspace
syn move <id> --to-preference    # should apply everywhere
syn pin <id> / syn unpin <id>    # keep it in the session-start digest
syn show <id> / syn list
```

`syn move` keeps the id, the content and the original creation date, so prefer it
to forget-and-re-save when a synapse is merely in the wrong place. Its source
flags say where the synapse *is* (`--workspace`, `--preference`), its `--to` flags
where it belongs.

Every command acts on one store, so say which — read it off the hit line:
`(work · fresha/offers, …)` → `--workspace work`; `(preference, …)` →
`--preference`. Omit the flag and it uses the workspace resolved for the current
directory, which is right when acting on a hit from that same workspace.

**When the server is down**, reads fail fast with a one-line error — carry on
without synapses, do not retry in a loop. Writes queue locally and report
`queued … not yet recallable`, flushing on the next successful command. Say so
plainly rather than claiming it was stored. `syn list --pending` shows the queue.
