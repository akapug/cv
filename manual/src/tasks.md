# The task substrate

Tasks let a fleet **commit to work** — and let a human see what has been said about that work,
by whom, when. 🔮

A **task** is a durable dispatch object: open it, claim it, note progress on it, finish it. A
**decision** is a task with a posed question: one event poses it (with the default that stands if
nobody speaks), one event answers it.

Storage is one append-only event log — `$CLUSTERVISION_HOME/tasks/events.jsonl` — under a
crash-safe flock. Every state you ever see is **replay-derived**: fold the
events through a pure reducer and you get the truth; there is no second store to drift out of
sync. The substrate is exposed three ways, all rendering the same shapes: the `cv task` CLI
(this page), the [MCP `task_*` tools](mcp.md#the-task-substrate-over-mcp), and
[`cv serve`'s task routes](serving.md#task-endpoints), whose inbox page (`/inbox`) is the human's door.

## The three laws

1. **cv never claims to have observed what it has not.** A completion is the agent's own word.
   `cv task done --observed <evidence>` records that word and labels it `self-reported`; nothing
   in cv turns it into a verified fact. What git and the filesystem can say about a task is a
   [`sweep`](#the-read-surfaces) candidate, never a close.
2. **No authority machinery.** No seats, grants, tokens or debts. Identity is `--from` /
   `CV_ENDPOINT`: recorded, never authenticated.
3. **Small.** This substrate stays a few thousand lines. Complexity requests go to the
   design-notes graveyard, next to the 200k-line system this replaced.

> Until 0.15.0 the substrate also carried a **land facet** — reviewed code revisions whose
> landing a git verifier observed — with its own trust layer (TOFU tokens, reviewer receipts,
> completion checks, a heartbeat). In 84 days of 8–24-lane swarms it was used on the day it was
> built and never again, so it was removed: `propose`, `pass`, `refute`, `reroute`, `verify`,
> `debt`, `stats`, `--token`/`CV_TOKEN` and `--check-*` error with a pointer. A log that holds
> the old events still replays; they are state-neutral.

## The base lifecycle

Every task has a base lifecycle:

```text
open ──claim──► claimed ──done──► done
  ▲                │
  └───release──────┘         (abandon / supersede: terminal from open or claimed)
```

- **`open`** — anyone can open a task (`cv task open <title>`), optionally with `--repo`
  (what `list --repo` filters on and `sweep` reads), `--assignee`, `--issue` and a `--body`.
- **`claim` / `release`** — claiming is a durable, race-free **first-writer-wins**: the store
  validates each append against a replay of the log *under the lock*, so when N agents race,
  exactly one claim lands and the rest get a rejection, not a duplicate.
- **`note`** — progress notes; never change state. **A note is accepted on a terminal task too**:
  terminal is about *state*, not the record. A note appended after `done` / `resolved` /
  `abandoned` / `superseded` is marked `post_close: true` and every surface says so — `show`,
  the inbox Markdown and the web page render it "(after close)" — and the task's state does not
  move (pinned by `reduce.rs::post_close_note_appends_and_never_changes_effective_state`).
  Four lanes and the orchestrator lost or re-homed evidence before this, because they wrote
  `done` first and the note second. Tags stay refused on a terminal task: they change views.
- **`done`** — completes a task, optionally pointing at observable evidence
  (`--observed <url-or-path>`). `--note TEXT` / `--note-file F` (and the same on `abandon`)
  appends the note and the terminal event as **one unit** — validated together under the lock
  and written with one `write`, the note first: both land or neither does. `done` is
  **self-reported** and `show` says so (`done: self-reported 4m ago (tests green)`); it is
  refused on a decision, which is answered with `resolve`.
- **`status`** — `cv task status <id> --file F` (or `-` for stdin, or the text inline) pins the
  task's **STATUS**: one `status_set` event that *replaces* the pinned status in the projection
  (every earlier one stays in the events; `revisions` counts them). It is the relay hand-off
  field — a generation's tip, what is merged, what runs (pids and paths), what is next — so a
  successor reads one field, not the newest of a hundred notes. `show` prints it first, under
  `STATUS`; `show --brief` on a task with a STATUS prints only the title and the STATUS; `show
  --status` prints just the text (exit 1 when none is pinned). Unlike a note it is
  state-bearing: allowed on an open or claimed task, **by its assignee or its opener** (an
  identity-bearing verb), and refused on a terminal task.
- **`abandon` / `supersede`** — the terminals. Abandon is always available on a non-terminal
  task.

Task ids are time-sortable UUIDs (v7) and, as everywhere in clustervision, a **unique prefix
is enough** on the command line. Because v7 ids opened within the same second share their first
eight hex digits, every list renders ids at the shortest length that keeps them distinct (never
below eight) — a batch of 23 tasks opened by one orchestrator used to print as 23 copies of
`01a0f52e`, none of which `show` would accept.

### Decisions, and provisional resolutions

A **decision** is a task with a posed question:
`cv task decide "<title>" --for <who> --default <option> [--option <alt>]… [--by <when>]`
appends `opened` + `posed` as one unit — one event creates the decision, one answers it — and
the decider answers with `cv task resolve <id> --accept-default | --choice "<option>"` —
identity-bearing, terminal (`resolved`). `done` is refused on a decision; a second `posed` is
refused.

**There is one way to pose a decision.** A `decision` tag on a task with nothing posed is
**refused by the reducer** — on the CLI, over MCP and from the page alike — with the message
naming `cv task decide` and its required `--default`: a tag-only decision gave the decider an inbox
card with no options and no buttons (two of them, 10-01). A tag-only decision already in a
store reads as the plain task it is (an assigned action), and nothing is migrated. `cv task
split`, which mined `DECIDE:` notes into decisions, is gone with it: pose the question.

**Provisional resolutions** are how an orchestrator proceeds on a default without leaving the
decision to rot as owed: `cv task decide … --provisional` (or
`cv task resolve <id> --accept-default --provisional` on an existing one) resolves it **on its default, by the poser**,
with a veto window. The task is `resolved` (work proceeds), `resolution.provisional` is `true`,
and the decider's inbox lists it in its own group, **made for you (veto?)** — not counted as owed.
The decider answers with `cv task resolve <id> --confirm` (keep it) or `--choice "<other>"` (veto),
from the page's "confirm" or alternative buttons too. That answer is **the one state-bearing event
a terminal task accepts**, and the rule is narrow: a *non-provisional* `resolved`, *by the
decision's assignee* (`web:<who>` counts as `<who>`), *over a provisional resolution*. It
replaces the resolution; the provisional one is kept as `decision.superseded_provisional` (the
last non-provisional resolution wins). A third party's override, a provisional choice other than
the default, and a second non-provisional resolve are all refused
(`reduce.rs::provisional_resolution_is_overridden_only_by_the_decider`).

### Bodies, tags and relations

- **`--body-file <path>`** (`-` for stdin) reads the body from a file; `note --file` does the same
  for notes. A brief is a document, not a 500-character shell string.
- **`--tags a,b`** labels a task; `list --tag <t>` filters on a label, and `cv task tag <id> a,b`
  adds labels later (additive, never removes). One label is reserved: **`decision`** is refused on
  a task with nothing posed (see *Decisions* above). `open --tags --blocked-by --blocks` is one
  unit: the task and its labels and relations land together or not at all.
- **`--blocked-by <id>`** records that this task waits on another; **`--blocks <id>`** records the
  same relation on the *other* task (`cv task block <id> --by <blocker>` after the fact). Both are
  resolved before the open is written, so a typo refuses the command rather than opening a task
  with half its relations. Whether a task is blocked *now* is computed when you look — any
  blocker still in a non-terminal state — so a blocker finishing, or being abandoned, unblocks
  without another event. `list` marks blocked rows with `⊘`; `show` prints `blocked by:` (with
  each blocker's state) and `blocks:` (the reverse).
- **`list --tsv`** prints one tab-separated row per task — full id, state, assignee, repo
  basename, age, title, blocked_by — for `cut`/`awk`/`sort`.

Tags and relations are events like everything else (`tagged`, `blocked_by`), folded by the
reducer into `tags` / `blocked_by` on the projection; both are omitted from the wire when empty,
so a log that never used them serializes exactly as before.

## The log is loud, never silent

Reads are best-effort and **loud** — an interior line the reducer refuses is quarantined with a
warning naming the line, and every surface ships those warnings; appends **fail closed** on a
degraded log rather than validating against an incomplete model. Events from the retired land
facet (`revision_proposed`, `review_passed`, `landed`, …) replay as state-neutral events: the
log keeps them, `show --events` prints them, and nothing is derived from them.

## The read surfaces

**`cv task list`** — non-terminal tasks by default, oldest first, with an age column off the
last event. `--state <s>` filters by state, and a typo is an error naming the whole vocabulary,
never a silently-empty list:

```text
Error: unknown state "redy" (expected one of open|claimed|done|abandoned|superseded|resolved)
```

**`cv task inbox [who]`** — "what needs me", **grouped by reason and stalest first within a
group** (age is the escalation mechanism; there is no other). Bare `cv task inbox` means *my*
inbox via `$CV_ENDPOINT`. The groups print in this order — `decisions owed`, `in discussion`,
`made for you (veto?)`, `assigned actions`, `claimed work` — because a decision nobody sees is
the slowest blocker a fleet has. Each reason has an honest aging anchor:

| Reason | You appear because | Ages since |
| --- | --- | --- |
| `DecisionOwed` | a live posed decision is assigned to you | the **pose** |
| `Discussing` | you parked a decision for discussion — open, not owed | the last event |
| `Provisional` | a decision assigned to you was resolved *for* you, provisionally — resolved, not owed | the **provisional resolve** |
| `AssignedOpen` | an open task is assigned to you, unclaimed | the last event |
| `ClaimedByYou` | you claimed it; it's yours to finish | the last event |

Rows waiting more than 24 hours lead with a `⏰`.

**`cv task sweep --repo <path>`** — candidates for closing, observed rather than asserted:
every non-terminal task (in that repo, or with no repo) whose `--issue` path no longer exists
(tried against the task's repo, the swept repo, the directory above it and the cwd), or whose
title/body names a branch now merged into the repo's main (`git branch --merged`, local and
remote-tracking; a name has to look like a branch — a separator, a digit, or eight characters —
so a branch called `fix` does not sweep every task that says "fix"). It prints them as *probably
done* with the reason and **never closes one**; `done --observed` or `abandon --reason` is still
a decision someone makes having read why.

All of these take `--json` for the full wire shapes (the same rows MCP and the HTTP API serve).

## Identity: `CV_ENDPOINT`

The fleet convention: the spawner sets `CV_ENDPOINT=agent:<name>` in each agent's environment,
and every cv front-end derives its default actor from it — the *bare* command records the
right identity. Nothing verifies the string (law 2); it exists so a forgotten flag can't
misattribute the durable record forever.

- **Chat-grade verbs** (`open`, `note`, `done`, `abandon`, `supersede`) fall back
  to a deliberate default sink — `cv` on the CLI (a human at a shell owes no ceremony),
  `agent` over MCP.
- **Identity-bearing verbs** — `claim`, `release`, `status`, `resolve`, whose endpoint string
  *keys inbox and decision semantics* — refuse to run without an identity, naming the cure:

```text
$ cv task claim 019f6e4c
Error: set CV_ENDPOINT or pass --from; identity-bearing events must record who acted
```

## A worked example

An orchestrator and one lane. (Transcript from a real run; the repo path is abbreviated.)

```text
$ export CV_ENDPOINT=orchestrator:o
$ cv task open "switch task ids to uuid v7" --repo ~/code/demo --assignee lane:mira
✦ opened 019f6e4b → open
019f6e4b-74c5-74f0-a493-1f8131ca38d5

$ CV_ENDPOINT=lane:mira cv task claim 019f6e4b
✦ claimed 019f6e4b → claimed
$ CV_ENDPOINT=lane:mira cv task note 019f6e4b "ids are v7 on task/uuid-ids; tests green"
✦ noted 019f6e4b → claimed
$ CV_ENDPOINT=lane:mira cv task done 019f6e4b --observed "task/uuid-ids @ c889a02, cargo test green"
✦ done 019f6e4b → done
```

The completion is the lane's own word, and `show` labels it so:

```text
$ cv task show 019f6e4b
task 019f6e4b-74c5-74f0-a493-1f8131ca38d5  [done]  action
  done:     self-reported 2m ago (task/uuid-ids @ c889a02, cargo test green)
  title:    switch task ids to uuid v7
  repo:     /Users/you/code/demo
  channel:  #tasks
  assignee: lane:mira
  opened:   2026-07-17 00:17 by orchestrator:o
  note (lane:mira, 2026-07-17 00:31): ids are v7 on task/uuid-ids; tests green
```

Later, `cv task sweep --repo ~/code/demo` lists every still-open task that names a branch now
merged into `main` — what git can say, offered as a candidate, closed by a person.

`cv task events --kind <kind>` rejects unknown event kinds with exit 2 and lists the known
kinds; `/api/events?kind=` returns 400 for the same mistake.

`cv task show <id> --events` (or `GET /api/task/{id}/events`) prints the raw durable history —
for this task: `opened`, `claimed`, `noted`, `done`.

## Growth & retention

Honesty section: `tasks/events.jsonl` is **append-only and currently unbounded** — every event
ever appended stays in the file, and every read replays all of it. At fleet-task volumes that
is cheap for a long time, but it does grow monotonically. Compaction is planned as
**snapshot + tail**: fold the log's stable prefix into a read-model snapshot, keep the live
tail as raw events, and retire the prefix to an archive file — preserving the audit prefix
(retired events stay on disk and verifiable; they just stop being replayed on every read).
Until that lands, the log is a single file you can archive by hand, and replay cost grows
linearly with event count.
