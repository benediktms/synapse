---
name: synapse
description: Durable cross-session synapses via the `syn` CLI — the store for anything that should outlive this session. Reach for it whenever the user talks about memory in words of their own, whatever they call it: "remember this", "don't forget", "note that down", "make a note", "keep that in mind", "memorise it", "save that", "for future reference", "do you remember", "did I tell you", "what do you know about X", "forget what I said", "update what you know". Read it before answering from assumption, whenever the user names a program, service, convention, or decision you cannot account for from the repo in front of you, or points backwards ("like we discussed", "the usual way", "as before"), or before starting substantive work on a subsystem. Write to it, unprompted, the moment a decision is reached, a preference is stated, or you are corrected — durable facts only, never tasks or reminders. A user who says "remember" is asking you to make something durable, not telling you how far it should reach: reach is a judgement made per fact on `syn save --scope`, and `--scope everywhere` is only for facts that stay true in a codebase with nothing to do with this one. After any write, tell the user the memory id `syn` printed and the reach it went to, in one line — that id is what they need to fix a mis-scoped fact. This is the store to reach for even if the harness also offers a memory directory of markdown files — synapses carry a scope that such files cannot express, so use `syn` and do not mirror the same fact into both. Use it even when a session-start digest has already appeared: the digest is a few lines out of the whole store, not a substitute for querying it.
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
- **The user asks what you know** — "do you remember", "did I tell you about X",
  "what do you know about the deploy flow". A question about the store is answered
  from the store. Answering it from whatever happens to be in this session's
  context tells them they never said it, when in fact they did and it is filed.
- **Before any write** — see step 2.

Skip it when the question is fully answered by code in front of you. Recall on
every trivial turn is noise, and noise is what gets the habit dropped.

Hits name their origin — a workspace, `workspace · project`, or `everywhere`:

```
[m_7f2a] (work · fresha/offers, 2026-07-14) Staging deploys for offers-service go
through ArgoCD app `offers-stg`, NOT the deploy Slack bot.
[m_31bc] (everywhere, 2026-06-02) Benedikt prefers deploy verification via a
Datadog dashboard link over log tailing.
(2 results, 140ms)
```

## 2. Pick the move

A store that only grows becomes noise, and once the user stops reading the digest
you have lost the channel, not just one fact. So recording is tending a network,
not appending to a log. Five moves:

| Move | Command | When |
| --- | --- | --- |
| **Grow** | `syn save` | nothing covers this yet |
| **Strengthen** | `syn edit <id>` | a synapse is nearly right and this sharpens it — two half-true synapses on one topic are worse than either alone |
| **Prune** | `syn forget <id>` | a decision made an old synapse false; a retirement invalidates synapses, it does not merely add one |
| **Rewire** | `syn move <id>` | true, but filed where the sessions that need it will never look |
| **Link** | `syn relate/support/contradict`, `syn supersede` | recall surfaces a memory this fact is genuinely related to — neither Strengthen nor Prune applies, but the two belong together |

Recall the topic first. Not as a rule to obey — it is the only way to know which
of the five you are in.

### Linking memories

When recall surfaces a memory this fact is **genuinely related to** — not merely
same-topic, but actually connected — and neither Strengthen nor Prune applies,
record the relationship instead of leaving it implicit. Linking is how the graph
becomes traversable: a recall on one memory surfaces its linked neighbours, and
`syn links <id>` walks the graph, so a related memory is reachable even when
semantic search would never surface it.

Pick the edge type by how confident you are about the nature of the relationship.
When unsure, default to the weakest claim rather than guessing a specific one:

- `syn support <A> <B>` — one memory is evidence for the other (positive).
- `syn contradict <A> <B>` — the two memory disagree; a hint one may be stale.
- `syn supersede --old <B> --new <A>` — the new fact replaces the old one (directed;
  the superseded memory stops surfacing on its own but stays reachable).
- `syn relate <A> <B>` — related in a way you can't characterise further. Use this
  by default; upgrade to a specific type only when you're confident of it.

The relevance bar is the guard against a noise bucket: link only when the two are
**genuinely related**, not merely on the same topic. A `relation` edge between
anything topically adjacent is how the graph drowns in edges nobody reads. When
the relationship is real but a specific type feels forced, `relate` is the honest
move — a weak-but-real edge beats an overstated one.

Undo with `syn unlink <A> <B>`. The map owes it to the graph, not to cleanliness:
a link is cheap to create and cheap to remove, and both remain searchable either
way.

## 3. Pick the scope

Scope is what a plain memory file cannot express, and getting it wrong is the most
common way a store goes bad. Decide it **per fact**: two facts in one turn often
belong in two different places.

One flag carries it. `--scope` is the whole axis, widest to narrowest:

```
syn save "<fact>" --kind feedback --scope everywhere   # every workspace, every project
syn save "<fact>" --kind decision --scope workspace    # every repo in this workspace
syn save "<fact>" --kind decision                      # this repo (inferred from git origin)
syn save "<fact>" --kind reference --scope owner/repo  # a repo you are not standing in
```

The question that picks the line: **would this still be true, and still worth
knowing, in a codebase that has nothing to do with this one?**

- **Yes → `--scope everywhere`.** Facts about the user and how they want to be
  worked with: communication style, tooling choices, blanket rules like "never
  force-push shared branches".
- **No, it concerns this body of work → `--scope workspace`.** It names the user's
  own programs, repos or conventions and spans more than one of them. A decision
  about how two of their projects relate belongs to neither alone, and filing it
  under one hides it from the other. Also correct when the checkout has no git
  remote, since there is nothing to infer from.
- **No, and it is one repo's business → omit `--scope`.** It comes from the git
  origin. Name `owner/repo` to file against a project you are not standing in.

A useful tell: a fact that names a program is almost never `--scope everywhere`.
Project architecture is not a preference.

### The user's wording does not decide the scope

"Remember that", "note this down", "don't forget", "keep that in mind" all say
*make this durable* — none of them says how far it should reach, and the person
saying it is not thinking about reach at all. They are handing you a fact, not a
filing instruction. So a request containing the word remember still comes through
this step: ask the question above about the fact itself, then pick a line.

A real one that went wrong: *"the frontend repo is app-b2c-spa, remember that —
generally on the marketplace when we say clients we mean web, android and ios"*
was filed everywhere because of that one word. It names three of one employer's
repos, so it was `--scope workspace`; globalised, an employer's convention now
surfaces in unrelated personal projects forever. The give-away was in the sentence
all along — a repo name.

It holds in reverse too. "While you're in this repo, stop asking before running the
tests" sounds local because of where it was said, but it is about how they want to
be worked with, so it is `--scope everywhere`. Neither the phrasing nor the current
directory is the signal; the fact is.

**When torn, `--scope workspace` is the safe middle.** The two mistakes are not
symmetrical, and neither is cheap. Globalised, a fact surfaces in every unrelated
session forever. Narrowed to one repo, it is invisible from every other repo —
recall filters project-scoped memories to the project you are standing in, and
`--all-workspaces` does not lift that. Workspace scope is reachable from anywhere
in that slice of the user's life while staying out of the others.

## 4. Write

```
syn save "<fact>" --kind user|feedback|decision|reference [--scope …] [--tags a,b]
```

`--kind` records what kind of fact it is: `feedback` for a correction or a stated
way of working, `user` for something about the person, `decision` for a call the
code will not record, `reference` for a URL, dashboard, ticket or runbook. It is a
separate axis from `--scope`: a `feedback` fact can be one repo's business, and a
`decision` can hold everywhere.

Write in the same turn it happens. A decision is durable the moment it is reached;
deferring to a tidier moment is how it gets lost. Do not ask permission — a write
is cheap and `syn forget` undoes it, whereas a decision never written costs the
user the whole argument again in six weeks.

**Report each write in one line, and always include the id `syn` printed**, after
your answer: `saved [m_00B1] (work) brain retired → repo-link + synapse`. Not a
section, not a justification. The id is the part that matters — it is what the user
types into `syn forget` or `syn move` when the scope is wrong, and a write reported
without one leaves them having to search for their own memory to correct it. Name
the reach too, since that is the judgement most worth checking.

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
  how the user wants to be worked with, is `--scope everywhere`; one repo's
  convention takes the default repo scope, or `--scope workspace` if someone could
  need it while standing elsewhere.

Write them self-contained: absolute dates ("2026-08-03", not "yesterday"), full
names, enough context to be read cold in six months by someone who was not here.
A synapse is read by a stranger and cannot lean on anything that was only true in
the conversation that produced it.

## Reference

```
syn edit <id> --content "<corrected fact>"
syn forget <id>
syn move <id> --to <workspace>   # filed in the wrong workspace
syn move <id> --to everywhere    # should hold in every workspace
syn pin <id> / syn unpin <id>    # keep it in the session-start digest
syn show <id> / syn list
syn relate <A> <B>               # generic relationship (default)
syn support <A> <B>              # one backs the other
syn contradict <A> <B>           # they disagree
syn supersede --old <B> --new <A>  # the new fact replaces the old
syn unlink <A> <B>               # drop whatever link(s) exist between them
syn links <id> --depth N         # walk the graph around a memory (JSON, JGF v2)
```

`syn move` keeps the id, the content and the original creation date, so prefer it
to forget-and-re-save when a synapse is merely in the wrong place. Its source
flags say where the synapse *is* (`--workspace`, `--scope everywhere`), its `--to`
says where it belongs.

Every command acts on one store, so say which — read it off the hit line:
`(work · fresha/offers, …)` → `--workspace work`; `(everywhere, …)` →
`--scope everywhere`. Omit the flag and it uses the workspace resolved for the
current directory, which is right when acting on a hit from that same workspace.

**When the server is down**, reads fail fast with a one-line error — carry on
without synapses, do not retry in a loop. Writes queue locally and report
`queued … not yet recallable`, flushing on the next successful command. Say so
plainly rather than claiming it was stored. `syn list --pending` shows the queue.
