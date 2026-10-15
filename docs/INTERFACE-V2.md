# Interface v2 (cv 0.11.0) — the clean break

Decided with ember, 2026-09-19: **clean break** (no aliases for old names), **snake_case
everywhere**, **full IR restructure**. This document is the contract every work package builds
against. When code and this document disagree during the initiative, fix the code; when the
document turns out to be wrong about a harness, fix the document in the same commit as the code.

Principle: **one name, one meaning, no grammar to memorize.** A verb names one act; a flag means
the same thing on every command that has it; a JSON key is spelled one way; a message has a
`kind`, a block has a `type`; harness-specific data lives under the harness's own key.

---

## The shape

*From the 2026-10 parsimony review, which measured cv at 110K lines growing 20K a month and cut it
toward this. It is the direction every change steers by: a change that moves away from it needs a
reason written next to it.*

**Three crates, one binary, one API, one UI, ~30 verbs, two reshape primitives.**

- `cv-core`: IR, adapters, emit, the read API that every door maps onto, tasks, lanes, watch.
  `cv-search` stays the one satellite only because tantivy does not belong inside the dependency
  fence. `cv-web` is the wasm cdylib.
- `cv`: `cv <verb>`, `cv mcp`, `cv serve`, `cv watch`. The former `cvd` and `cv-mcp` binaries
  are these subcommands. The MCP roster is generated from the in-process clap tree, so it cannot skew from the
  CLI (it once shelled out to whatever `cv` was on PATH, and found a stale one).
- **An adapter is `roots + discover + record-to-Message stream`, 150 to 400 lines**, importing
  its wire codec (`harness/wire/{anthropic,openai}`) and its store helper
  (`harness/store/{jsonl,sqlite}`). Emitters live beside their readers and are the codec run
  backwards plus a store writer. Grok, at 500 lines, is the model, not Claude.
- **Two reshape primitives, each spelled once.** Record-level and lossless for Claude:
  `derive(src, Select, Rewrite)`, where prune, rewind and adopt are 30 to 80-line selectors.
  IR-level for any harness in: `select messages, then emit(Claude)`, where the emitter itself
  guarantees linear parents, no orphan `tool_use`, correct sidechain identity and the sidecar, so
  fork, distill, splice and `pack --format session` are 20 to 50-line selectors and `loom`,
  `linearize`, `restamp` and the emit-read-rewrite dance do not exist.
- **A task is an append-only log of about eleven event kinds** folded by a ~450-line pure
  reducer under one CAS. A decision is a task with a `posed`; one event poses, one answers,
  provisional-then-confirm is the one override. Completion is self-reported and labeled so.
  Lanes are a pure function of the session directory, never stored. One wake primitive
  (`Watcher` with a cursor), one notify channel (`task events --since`), no board.
- **One hand-written guide of about twelve chapters**, one generated command reference from
  `cv schema --commands --json`, one contract with a test that fails when its command table
  disagrees with the clap tree, a CHANGELOG whose every heading is a tag, tests where the rule
  lives (reducer rules in the reducer, renders beside the command, one `cv-testkit` World, a
  ~1,200-line CLI smoke file).

A session mid-work asks: is this a `--kind`, a `--sort`, a selector, a route on the one API, or
a reducer event? It is never a new top-level command, binary, server, renderer, scanner or
harness.

---

## 1. Commands

Grouped help (`cv --help` shows every command under these headings; nothing is hidden).
**Read** commands are read-only over existing sessions; **Reshape** commands produce a NEW
session id from existing ones and never touch the source (`resume` launches one); **Export**
commands produce something that is not a session; **Fleet & live** is multi-agent coordination,
live views and the servers; **System** is cv's own state and reference.

The two tables below are generated from the clap tree, and a unit test
(`cmd::schema::tests::contract_tables_are_the_clap_tree`) fails when they disagree with it;
`CV_REGEN_CONTRACT=1 cargo test -p clustervision --bin cv contract_tables` rewrites them.

<!-- begin: cv schema --commands -->
| group | commands |
|---|---|
| **Read** | `ls` `show` `cat` `search` `events` `touched` `tools` `workflow` `lanes` `deferrals` `stats` `blame` `doctor` |
| **Reshape** | `prune` `rewind` `distill` `adopt` `splice` `port` `resume` |
| **Export** | `export` `dataset` `pack` |
| **Fleet & live** | `task` `scry` `watch` `archive` `serve` `mcp` `share` |
| **System** | `index` `config` `schema` `formats` `recipes` `hooks` `hook` |

| command | subcommands |
|---|---|
| `task` | `open` `list` `show` `claim` `release` `note` `done` `status` `abandon` `supersede` `tag` `block` `decide` `resolve` `events` `sweep` `inbox` |
| `archive` | `sync` `ls` `path` |
| `hooks` | `install` `remove` `show` |
<!-- end: cv schema --commands -->

Renames and removals (old → new; the old name is GONE, it errors with a pointer):

| old | new | why |
|---|---|---|
| `convert <id> --to <h>` | `port <id> --harness <h>` | convert and port were the same act (produce a copy that runs elsewhere); one verb |
| `port --to <h>` / `--to-dir <d>` | `port --harness <h>` / `--cwd <d>` | say what the value is, not where it goes |
| `query` | `schema` | `query` prints the field/operator reference; it does not run a query |
| `recall` | removed | superseded by `pack` (`pack` is the one "build context from the corpus" verb). `distill` was removed with it in 0.11 and returned in 0.14 with a new meaning (a deterministic lane reshaping, not an LLM digest) |
| `prune --retrieve <id>` | `cat <session> <tool_use_id>` | fetching a tool's output is a Read, not a prune option |
| `prune --range` | `prune --keep A..B` | it selects what to KEEP; `--range` is the read-window flag elsewhere |
| `show --range -N` | `show --first N` | `-N` looked like "last N" to everyone who used it |
| `show --pre-compaction [N]` | unchanged | |
| `cv help` footer | `recipes` | the agent quickstart is a real command |

New:

- **`cat <session-id> <tool_use_id>`** — print one tool call's full output, wherever it lives:
  inline in the transcript, in a `prune` sidecar, or in a persisted-output file
  (`<session>/tool-results/…`, Kimi `tool-results/*.txt`). `--input` prints the call's arguments
  instead. Exit 2 if the id is unknown.
- **`formats`** — the format census (see §7): `formats [--harness h] [--recent N] [--json]` lists
  unknown record/part/column vocabulary seen in real data. The manifest ↔ source comparison is the
  test `cargo test -p clustervision-core --test formats_manifest`, not a command.
- **`recipes`** — the agent quickstart: the things agents do with cv, each as one command
  line with its JSON shape. Also printed by `cv --help`'s last line as "run `cv recipes`".

## 2. Flags — the same word means the same thing everywhere

**Windows** (on `show`, `export`, `cat --input` n/a, MCP `show`; all 0-based, end-exclusive):

| flag | meaning |
|---|---|
| `--first N` | the first N messages |
| `--last N` | the last N messages |
| `--range A..B` | messages A (inclusive) to B (exclusive); `A..` to the end, `..B` from the start |
| `--around N [--context K]` | message N with K messages either side (default 5) |
| `--max-bytes N` | stop after N bytes of rendered output and print a continuation line `… continue with --range <next>..` |

Only one of `--first/--last/--range/--around` may be given. `prune --keep A..B` uses the same
`A..B` grammar (turn indices, as today).

**Everywhere else:** `--harness <h>` (filter or target, never "--to"), `--cwd <dir>`, `--out <dir>`
(write somewhere other than the real store), `--json` (machine output; every command that prints a
list or a session has it), `--limit N`, `--fresh` (bypass the catalog), `--all` (include what is
hidden by default: finished lanes, archived sessions, terminal tasks).

**Ids:** every command accepts a unique id prefix or `harness:id`. An ambiguous prefix lists the
candidates as `harness:full-id` lines and exits 2.

## 3. JSON — snake_case, one shape per noun

- Every `--json` output and every MCP payload uses **snake_case** keys. `ls --json` changes:
  `messageCount → message_count`, `createdAt → created_at`, `updatedAt → updated_at`,
  `sizeBytes → size_bytes`, `displayTitle → display_title`. `search --json` and `workflow --json`
  likewise. Timestamps are `DateTime::to_rfc3339()` — UTC spelled `+00:00`, never `Z` — on every
  door, so the same instant compares equal whether it came from the CLI, the daemon or MCP.
- A **session row** (from `ls`, `search`, `project_sessions`, …) always has the same keys:
  `id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes` (+ `display_title`,
  `git` under `--enrich`).
- A **message** is the IR `Message` (below). A **block** is the IR `Block`, tagged by `type`.
- `cv schema --json` publishes the machine-readable schema of every shape above; `recipes` links it.

## 4. The IR (cv-core `ir.rs`)

```rust
pub struct Session {
    pub id: String,
    pub harness: Harness,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub git: Option<GitInfo>,
    /// The system prompt the harness sent (when it stores it: Hermes, Kimi Code, OpenClaw,
    /// Claude `prompt_snapshot`, Codex `base_instructions`). NOT a message.
    pub system_prompt: Option<String>,
    /// Where this session came from and where it went.
    pub lineage: Lineage,
    pub messages: Vec<Message>,
    pub source_path: Option<PathBuf>,
    /// Harness-specific session facts, nested under the harness name:
    /// `extra["claude"]["custom_title"]`, `extra["codex"]["history_mode"]`, … Never flat.
    pub extra: Map<String, Value>,
}

#[derive(Default)]
pub struct Lineage {
    /// The session this one was forked or branched from (Codex `forked_from_id`, OpenClaw fork,
    /// Hermes `_branched_from`, Claude `--fork-session`).
    pub forked_from: Option<String>,
    /// The session that owns this one as a sub-agent (Codex `parent_thread_id`, Claude parent
    /// session of an `agent-*` transcript, Kimi `parentAgentId`, Hermes `_delegate_from`).
    pub parent: Option<String>,
    /// The tool call in the parent that spawned this session (Claude `toolUseId`), if known.
    pub spawned_by_tool_use: Option<String>,
    /// The session this one continued in (Claude `continued-in`, Hermes compression rotation).
    pub continued_in: Option<String>,
    /// The session this one continues (the inverse pointer, when the store records it).
    pub continues: Option<String>,
    /// Sub-agent path or nickname when the harness has one (Codex `agent_path`, Kimi `agent-N`).
    pub agent_path: Option<String>,
}

pub struct Message {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub role: Role,          // System | User | Assistant | Tool — WHO speaks
    pub kind: MessageKind,   // WHAT the message is — see below
    pub origin: Origin,      // WHERE it came from
    pub timestamp: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    /// Harness-specific message facts, nested under the harness name: `extra["claude"]["attachment_type"]`.
    /// Exactly one other key is allowed at the top level: `"_record"` (the verbatim carrier record
    /// under `ParseOptions::complete`).
    pub extra: Map<String, Value>,
}

/// What a message IS. Every adapter sets it; every emitter reads it.
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A human's typed prompt (Role::User, Origin::Human).
    Prompt,
    /// The model's reply (Role::Assistant): text, thinking and tool calls.
    Reply,
    /// Tool output fed back to the model (Role::Tool).
    ToolResult,
    /// Context the HARNESS injected into the model's input: Claude attachments/system reminders,
    /// Kimi injections, Goose `<turn-context>`, Codex `<environment_context>`, hook stdout.
    InjectedContext,
    /// The system prompt, when the store keeps it as a message (Kimi `profile.bind`, Hermes system
    /// row, OpenClaw). Session-level copy in `Session::system_prompt`.
    SystemPrompt,
    /// A harness notice shown to the user, not sent to the model: slash-command output, task
    /// terminated, turn aborted, "usage limit reset", Codex `item_completed` notes.
    Notice,
    /// The point where the harness compacted; `compaction` carries the metadata.
    CompactionBoundary,
    /// The summary that seeds the next window after a compaction.
    CompactionSummary,
    /// The model (or effort) changed from here on; `model` holds the new one.
    ModelChange,
    /// An error the model never answered: API error, refusal fallback, retry exhaustion.
    Error,
    /// A sub-agent was spawned (the `Agent`/`spawn` call); `lineage`-like pointers in `extra[h]`.
    SubagentSpawn,
    /// A sub-agent's final return delivered to the parent.
    SubagentReturn,
    /// A branch/rewind/reset marker: what follows does not continue what precedes.
    Branch,
    /// A verbatim non-conversational record carried only under `ParseOptions::complete`.
    Carrier,
}

#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,      // typed by a person
    Model,      // produced by the model
    Harness,    // produced by the harness itself (reminders, notices, compaction)
    Hook,       // a user-configured hook's output
    Scheduler,  // cron / loop wakeups, automation
    Subagent,   // another agent (inter-agent messages, sub-agent returns)
    Import,     // imported from another harness by the harness (Goose/Hermes importers)
    Unknown,
}

/// The four token counts are DISJOINT (Anthropic's convention): `input_tokens` is the uncached
/// prompt only, and the prompt the model read is input + cache_read + cache_creation
/// (`Usage::prompt_tokens`). Providers that report cached tokens as a subset of input (Codex,
/// Gemini) are normalized on parse and re-inclusived on emit, so sums across harnesses mean one
/// thing. `reasoning_tokens` is a subset of `output_tokens`.
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    /// Reasoning/thinking tokens when the provider reports them separately.
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported cost, when the harness stores it (Goose, OpenCode, OpenClaw — Codex
    /// records none).
    pub cost_usd: Option<f64>,
}

/// Content blocks — serde tag is `type` (the word every harness uses on the wire).
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text },
    Thinking { text, signature, encrypted, redacted },   // signature = Anthropic-bound, encrypted = OpenAI-bound
    ToolUse { id, name, input, namespace: Option<String> },
    ToolResult { tool_use_id, content, is_error, tool_name, status, details },
    Image { media_type, data_ref },
    File { mime, path, source },
}
```

Rules:

- **`kind` is mandatory and never guessed by consumers.** Defaults by role only for legacy
  construction (`Message::new(role)`): User→Prompt, Assistant→Reply, Tool→ToolResult,
  System→Notice. Adapters MUST set the precise kind.
- **`extra` is nested by harness, always.** `m.harness_extra_mut(Harness::Claude)` returns the
  `extra["claude"]` object. The keys inside keep the harness's own spelling (`attachment_type`,
  `history_mode`, `display_kind`), snake_case where cv invents a name. A namespace key is either a
  canonical harness name or `cv` (`ir::CV_NAMESPACE`, reached with `Session::cv_extra_mut`), which
  holds facts cv itself produced rather than read from a store: parse diagnostics such as
  `skipped_lines`, and the provenance cv stamps on a session it synthesized, such as `loom`.
  `Harness::parse("cv")` is `None`, so the two can never collide. **This rule binds cv's own
  session producers too, not only adapters** — `loom` wrote a flat `extra["loom"]` for a release
  and made this very sentence false; its tests now call `assert_no_flat_keys` like an adapter's. Exactly two flat keys survive, both message-level and both cv's own streaming
  bookkeeping, never harness facts: `_record` (the verbatim carrier under `ParseOptions::complete`)
  and `cv_byte_offset` (`crate::offsets::OFFSET_KEY`, on the per-message hot path). A session's
  `extra` has no flat keys at all. `harness::assert_no_flat_keys(&Session)` states this rule once
  and every adapter's tests call it.
- **Shared concepts leave `extra`.** Compaction → `kind`; system prompt → `Session::system_prompt`;
  parent/fork/continued → `Session::lineage`; tool details → `Block::ToolResult::details`;
  error → `kind: Error` + `extra[h]["error"]`; injected context origin → `origin`.
- **Emitters read kinds, never harness keys.** An emitter may consult `extra[<its own harness>]`
  when round-tripping into the same harness (format-complete replay), nothing else.
- **Format-complete replay fields live in the harness bag too.** Under `ParseOptions::complete`
  the Claude adapter mirrors a record's non-first-class fields into `extra["claude"]` (same keys
  as today, including the `message.`-prefixed ones) and the verbatim carrier record stays at the
  top-level `_record`. the Claude emitter reads them from `extra["claude"]`. Same pattern for every
  other harness that round-trips.
- **Shared core modules key off `kind`, not harness keys.** `compaction.rs` detects
  `MessageKind::CompactionBoundary`/`CompactionSummary`; `doctor.rs` buckets
  `MessageKind::InjectedContext` as system reminders (kind name from `extra["claude"]["attachment_type"]`
  when present); Claude's `toolUseResult` is a SHARED concept and lives on the block as
  `Block::ToolResult::details` (a `complete`-mode copy stays in `extra["claude"]` only for
  byte-exact same-harness replay);
  `render.rs`/`html.rs` and `cv show` label a System turn by its `kind` (plus the attachment kind).
- **`Session::model`** is the session default, and `Message::model` names the model that turn ran
  under. REARCH's "IR diet" would null a message model that merely repeats the session default;
  **that rule is withdrawn** (2026-09-19): a record's parse must not depend on how much of the
  file preceded it, and nulling a duplicate makes it depend on exactly that. It also leaves a
  windowed read with no way to learn the model at all when the adapter emits no `meta()`, which
  is the case for Claude. An adapter may still omit a model its
  store never recorded; it may not omit one the store did record. Redundant strings are the
  cheaper price than position-dependent parsing.
- **`events.rs`/`tools.rs`** reach harness facts through `harness_extra(Harness::X)`, never a
  flat key. The cautionary tale: the old seek store's codex model-change hazard check read a flat
  `codex_event` after the adapter had nested it, so it silently stopped firing.

## 5. Fidelity verifier v2 (`emit_verified`)

Replaces the six-count check. After emitting, re-parse the output and diff per message:
role sequence, kind sequence, block type counts, thinking (with text / signature-only / encrypted),
tool names on results, `is_error`, `details` present, usage present, model present, timestamps
present, ids present, plus session title, cwd, model, system_prompt, lineage. Report every
delta; classify each as **expected for this target** (from a per-emitter table of what the
format cannot carry, e.g. vendor-bound thinking, per-message ids) or **unexpected**.
`port --strict` fails on any unexpected delta; the default prints them under `⚠ lost`.

## 6. MCP (`cv mcp`)

Tools are **generated from the CLI**, not hand-written, so names, flags, harness lists and output
shapes cannot drift. Mechanism (the server is `cv mcp`, so the clap tree is in-process):

- `cv schema --commands --json` dumps the command tree: for every visible command and subcommand,
  `{ name, group, about, args: [{ name, kind: "flag"|"option"|"positional", value_type:
  "string"|"integer"|"number"|"boolean"|"path", long?, multiple?, possible_values?, default?, help, required }] }`,
  produced from clap's `Command` introspection (`CommandFactory`), including the window flags.
- `cv mcp` reads that tree from its own clap `Command` at startup (no binary to look up, so none
  to be stale) and registers one tool per Read/Reshape/Export/System command and subcommand and
  per `cv task` subcommand (a parent with no arguments of its own is not a tool), named after the
  command with a subcommand joined by `_` (`ls`, `show`, `task_open`, `hooks_install`, …); each argument becomes a JSON-schema
  property with the clap help as its description. Positionals map to properties named after the
  argument; repeatable arguments accept a scalar or an array. `long` carries the actual CLI flag
  spelling, which can differ from the argument's property name. A call runs the same executable as `cv <cmd> <args> --json` (or without `--json` for commands that have no
  JSON form, returning text) and returns stdout; a non-zero exit returns stderr as the error.
- Over MCP, transcript `show` defaults to `last: 50` and `max_bytes: 200000` when no window,
  byte cap or `pre_compaction` is supplied. A `kind` row view suppresses these defaults;
  `ls`/`search` default `limit` stays the CLI default.
- MCP-only tools keep their names and hand-written schemas: `observe_stream`, `await_omen` (one
  blocks, one holds a cursor). `read_session`, `search_sessions`,
  `list_sessions`, `project_sessions`, `recall`, `prune_session`, `prune_retrieve` are removed
  (their replacements are `show`, `search`, `ls`, `ls --cwd`, `pack`, `prune`, `cat`).

The roster, generated by the same test as §1:

<!-- begin: cv mcp tools/list -->
| group | tools |
|---|---|
| **Read** | `ls` `show` `cat` `search` `events` `touched` `tools` `workflow` `lanes` `deferrals` `stats` `blame` `doctor` |
| **Reshape** | `prune` `rewind` `distill` `adopt` `splice` `port` `resume` |
| **Export** | `export` `dataset` `pack` |
| **Fleet & live** | `task_open` `task_list` `task_show` `task_claim` `task_release` `task_note` `task_done` `task_status` `task_abandon` `task_supersede` `task_tag` `task_block` `task_decide` `task_resolve` `task_events` `task_sweep` `task_inbox` |
| **System** | `index` `config` `schema` `formats` `recipes` `hooks_install` `hooks_remove` `hooks_show` `hook` |
| **MCP-only** | `await_omen` `observe_stream` |
<!-- end: cv mcp tools/list -->

## 7. Format census and manifests (`cv formats`)

- **Census.** Under `ParseOptions::complete` every adapter carries unknown records as `Carrier`
  messages with `extra[h]["record_type"]`. `cv formats` parses recent sessions per harness
  in complete mode and reports, per harness, the record/part/column vocabulary seen with counts,
  marking anything absent from the manifest as **new**.
- **Manifests.** `formats/<harness>.toml`: `upstream = { repo, commit, date }`, `store = [...]`
  paths, and `[types]` with what the source cannot say: `generic | carried | ignored`, and
  `handled` only for an item no match arm names. An item the adapter matches on is `handled` by
  inference. A test asserts every listed `handled` type appears in the adapter source and that no
  entry restates a match arm.
- **Drift script.** `tools/harness-drift.sh` fast-forwards the `~/pug` checkouts and greps the
  persistence enums against the manifests (what the 2026-09-19 survey did by hand).
- **Fixture generators** live in `tools/harness-fixtures/<harness>/` (Goose stub provider + env,
  Hermes store script, OpenClaw driver, Codex resume smoke) so every real-writer fixture is one
  command to regenerate.

## 8. Migration notes

- **sesh** (`~/dev/sesh`, `server/catalog.py`, `server/sesh.py`) reads `cv ls --json`,
  `cv search --json` and `cv show --json --range`: update to snake_case keys and `--first/--last`.
- **ember's `~/.claude/CLAUDE.md`** references `cv workflow`, `cv scry`, `cv show/export`: all
  survive. `prune --retrieve` → `cat`.
- **Manual** (`manual/src/*.md`), `README.md`, `docs/FORMATS.md`, `CHANGELOG.md` (0.11.0 section
  headed "clean break").

## 8b. Where this is going (the attractor)

*Named 2026-09-20, in a parsimony pass over everything 0.11.0 added. Not a plan; a direction, so a
future session mid-work makes a different choice knowing it.*

cv keeps discovering the same idea and giving it a new name each time: **two representations of the
same thing disagree, and something must judge whether that is expected.** It is now implemented
six times, in four different shapes and two shapeless ones:

| the pair | the mechanism | the shape it reports in |
|---|---|---|
| manifest ↔ adapter source | `cargo test -p clustervision-core --test formats_manifest` | `formats::Finding { harness, kind, item, detail }` |
| manifest ↔ real on-disk data | `cv formats` | its own `RecordStat` |
| source IR ↔ emitted output | `emit_verified` | `emit::Delta { field, before, after, expected }` |
| render ↔ IR | `web/selftest.html` | `{ group, name, why }` |
| `extra` bags ↔ the namespacing rule | `assert_no_flat_keys` | a panic |
| manifest ↔ upstream checkout | `tools/harness-drift.sh` | printed lines |

Each checks a genuinely different pair, so none of them is redundant — this is not a call to delete
one. The cost is that **adding a surface means writing a seventh bespoke checker**, and that the
six cannot be read, filtered or rendered together even though they answer the same question.

**The end-state:** one `Divergence { what, a, b, expected, where }` and one renderer, so
`--strict`, `formats_manifest` and the dashboard all speak the same words, and adding
a surface means *declaring a pair* rather than writing a checker. `Finding` and `Delta` are already
that type with two different spellings; unifying them is the down-payment and the rest can adopt it
incrementally.

Two further steps toward it, both real debt found in the same pass:

- **This document had no mechanical check, and was factually wrong twice on the day it was
  written** (it claimed `events.rs` reads `toolUseResult` from the harness bag after that moved to
  the block, and listed Codex among the harnesses recording a cost, which it does not). §1 and §6
  are generated and tested now (2026-10). §3's key lists are still prose: either they get asserted
  against `cv schema --json`, or they shrink to only what cannot be checked. A contract nothing
  verifies decays into folklore.
- **Done (2026-10):** `handled` is inferred from the adapter's match arms; the manifests dropped
  the 242 entries that restated an arm and carry what the source cannot say — the `[upstream]` pin,
  and the `generic`/`carried`/`ignored` judgments.

## 9. Design history

The documents that argued their way to this contract live in git, not in `docs/`:

- `git show 5abb404:docs/REARCH.md` — streaming parse, lazy spans, the seek store (REARCH Phase 2), the withdrawn "IR diet".
- `git show ffcbefb:docs/PORTABILITY-REVIEW.md` — the source-backed harness audit that became `port --strict` / `emit_verified`.
- `git show c04695b:docs/DESIGN-GAP-AUDIT.md` — the task-substrate gap audit (G1–G11); what landed is in `manual/src/tasks.md`.
- `git show ffc3ef0:docs/design/DISTILL.md` — the distill/fork measurements on three real lanes; the rules are in `manual/src/reshaping.md`.
- The 0.11.0 work-package roster (WP 0–6, file ownership) is in this file's history: `git log -p -- docs/INTERFACE-V2.md`.
