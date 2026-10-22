# Architecture

This chapter is for contributors and the curious: how clustervision is built, and why it's shaped the way it is. 🔮

The whole project rests on one idea. Every harness — Claude Code, Codex, Gemini, Cursor, twenty-one of them — stores its sessions in a different on-disk dialect. clustervision parses every one of those dialects **into a single unified representation** (the IR), and then *everything else* — search, porting, packing, tasks, the app — is just a function over that IR. The data-flow one-liner is:

```text
parse(any harness) → IR → search · port · prune · loom · pack · archive · coordinate · view
```

Get the IR right and the rest composes. So we start there.

## The unified IR

The IR lives in [`cv-core/src/ir.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/ir.rs). It is deliberately a **superset** of what any single harness records: fields a harness doesn't have are `None`/empty. Three questions are answered by three fields on every message — **who** speaks (`role`), **what** the turn is (`kind`), **where** it came from (`origin`) — and every shared concept (compaction, system prompt, lineage, tool details, usage) has a first-class home, so conversions never have to guess from harness-specific keys. What is genuinely specific to one harness lives in `extra[<harness>]`, a bag keyed by the harness name — never at the top level.

The spine is `Session → Message → Block`:

```rust
pub struct Session {
    pub principal: Option<Principal>,           // the opaque identity the session acts as, when a store records one; never inferred
    pub id: String,
    pub harness: Harness,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,                  // the session default; each message carries the model it ran under
    pub git: Option<GitInfo>,
    pub system_prompt: Option<String>,          // the prompt the harness sent, when the store keeps it
    pub lineage: Lineage,                       // forked_from · parent · spawned_by_tool_use · continued_in · continues · agent_path
    pub messages: Vec<Message>,
    pub source_path: Option<PathBuf>,           // provenance: where it was read from
    pub extra: serde_json::Map<String, serde_json::Value>,   // extra["claude"], extra["codex"], … only
}

pub struct Message {
    pub id: Option<String>,
    pub parent_id: Option<String>,              // for branching / loom
    pub role: Role,                             // System | User | Assistant | Tool — WHO speaks
    pub kind: MessageKind,                      // WHAT the turn is (below)
    pub origin: Origin,                         // WHERE it came from: human | model | harness | hook | scheduler | subagent | import | unknown
    pub timestamp: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    pub extra: serde_json::Map<String, serde_json::Value>,   // extra["<harness>"] bag (+ "_record" under `complete`)
}
```

`MessageKind` is the vocabulary conversions and analyses key off:

| kind | meaning |
|---|---|
| `prompt` | a human's typed prompt |
| `reply` | the model's reply: text, thinking, tool calls |
| `tool_result` | tool output fed back to the model |
| `injected_context` | context the harness put in the model's input (system reminders, hook output, environment context) |
| `system_prompt` | the system prompt, when the store keeps it as a message |
| `notice` | a harness notice shown to the user, never sent to the model |
| `compaction_boundary` / `compaction_summary` | where the harness compacted, and the summary that seeds the next window |
| `model_change` | the model or effort changed from here on |
| `error` | an error the model never answered (API error, refusal, retry exhaustion) |
| `subagent_spawn` / `subagent_return` | a sub-agent was started here / its final return arrived |
| `branch` | a rewind, reset or fork marker: what follows does not continue what precedes |
| `carrier` | a verbatim non-conversational record, carried only under `ParseOptions::complete` |

`Message::is_model_visible()` includes prompts, replies, tool results, injected context, system prompts and compaction summaries, plus sub-agent spawns and returns carried by a non-System role. Harness System markers stay outside the model-visible exchange. `cv doctor`, the dataset exporter and the emitters all use it instead of reading harness keys.

`Origin` is the orthogonal question — not *what* the turn is but **where it came from**, which `role` alone can't tell you (a `user`-role turn might be a person typing, a hook's stdout, or a cron wakeup):

| origin | meaning |
|---|---|
| `human` | typed by a person |
| `model` | produced by the model |
| `harness` | produced by the harness itself: reminders, notices, compaction, environment context |
| `hook` | a user-configured hook's output |
| `scheduler` | cron / loop wakeups and other automation |
| `subagent` | another agent: inter-agent messages, sub-agent returns |
| `import` | imported from another harness by the harness (the Goose / Hermes importers) |
| `unknown` | the store does not say |

Both are **mandatory on every message and never guessed by a consumer.** `MessageKind::for_role` and `Origin::for_role` exist only so `Message::new(role)` and legacy IR JSON have a defined value (User→`prompt`/`human`, Assistant→`reply`/`model`, Tool→`tool_result`/`harness`, System→`notice`/`harness`); an adapter that leaves the default in place is a bug, not a shortcut.

### `extra` is nested by harness, always

Everything genuinely specific to one harness lives in `extra[<harness name>]` — `extra["claude"]["attachment_type"]`, `extra["codex"]["history_mode"]` — reached through `harness_extra(h)` / `harness_extra_mut(h)`, never written flat at the top level. The keys inside keep the harness's own spelling; where cv invents a name it is snake_case. Two message-level bookkeeping keys are allowed: `_record`, the verbatim source record under `ParseOptions::complete`, and `cv_byte_offset`, the source position under offset-enabled reads.

The rule that makes this work is that **shared concepts leave `extra`**. Compaction is a `kind`, not a Claude field. The system prompt is `Session::system_prompt`. Parent/fork/continuation pointers are `Session::lineage`. Structured tool-result state is `Block::ToolResult::details`. An error is `kind: error`. So `compaction.rs`, `doctor.rs`, `render.rs`, the dataset exporter and every emitter key off the IR, and an emitter consults `extra[…]` only for its *own* harness, when replaying a session back into the format it came from.

cv-defined JSON fields use snake_case; opaque principals, harness extras and native payloads keep their keys. [`cv schema --json`](cli.md#cv-schema) publishes the cv vocabulary machine-readably.

A `Message`'s content is a list of `Block`s — the atoms of a conversation. The enum is tagged `#[serde(tag = "type")]` (a block has a `type`; a message has a `kind`) so it round-trips to clean JSON:

```rust
pub enum Block {
    Text       { text: String },
    Thinking   { text: String, signature: Option<String>,        // signature: Anthropic-bound
                 encrypted: Option<String>, redacted: bool },    // encrypted: OpenAI-bound
    ToolUse    { id: String, name: String, input: serde_json::Value, namespace: Option<String> },
    ToolResult { tool_use_id: String, content: String, is_error: bool,
                 tool_name: Option<String>, status: Option<String>,
                 details: Option<serde_json::Value> },          // structured result state, attachments, persisted-output pointer
    Image      { media_type: Option<String>, data_ref: Option<String> },
    File       { mime: Option<String>, path: Option<String>, source: Option<String> },
}
```

(The text-bearing fields are shown as `String` above for readability. They are really `lazy::Text`, which may be a resolved string *or* a byte span into the transcript on disk — which is what lets a windowed `cv show` read only the bytes it prints, and lets a multi-gigabyte session be listed without materializing it. `Text` serializes as a plain JSON string either way.)

A few supporting types complete the picture:

- **`Harness`** — a `Copy` enum naming the source harness (`Claude`, `Codex`, `KimiCode`, `Grok`, `OpenCode`, `Gemini`, `Cursor`, `Goose`, …). It carries the canonical string forms: `Harness::ALL` lists every one, `as_str()` gives the lowercase name (also the key of its `extra` bag), and `parse()` accepts aliases (`cc` → `Claude`, `oc` → `OpenCode`, `kimi2` → `KimiCode`, …).
- **`Role`** — `System | User | Assistant | Tool`. We keep `Tool` distinct even though some harnesses model a tool result as a `user` turn, so conversions can re-encode it correctly.
- **`Usage`** — optional counts: `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `reasoning_tokens`, and `cost_usd` when the harness records a cost.
- **`Lineage`** — where a session came from and went: `forked_from`, `parent` (the owning session of a sub-agent), `spawned_by_tool_use`, `continued_in`, `continues`, `agent_path`.
- **`GitInfo`** — the `branch` / `commit` / `remote` a session ran against, when the harness records it.
- **`SessionRef`** — a *lightweight handle* to a session discovered on disk (`id`, `harness`, `path`, `cwd`, `title`, timestamps, `message_count`). It's cheap to produce for listings and search **without parsing the whole transcript**. Discovery yields `SessionRef`s; a full `Session` is parsed only when you actually open one.

`Session` also carries a little behavior for free: `label()` (a short title for listings, falling back to the first prompt by the one title rule, `Message::prompt_title`), `first_user_text()`, `searchable_text()` (every text/thinking/tool block concatenated; search indexing separately projects exchange and context), and `harness_extra(h)` / `harness_extra_mut(h)` for the per-harness bags.

## The `Adapter` trait

Every harness is one `Adapter` ([`cv-core/src/harness/mod.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/harness/mod.rs)). The trait is small on purpose:

```rust
pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;

    /// Every on-disk root this adapter reads (resolved against $HOME, existing ones only),
    /// the primary one first.
    fn roots(&self) -> Vec<PathBuf>;

    /// Cheaply enumerate sessions without fully parsing them.
    fn discover(&self) -> Result<Vec<SessionRef>>;

    /// Stream one session's messages into `sink`, one at a time; returns the metadata.
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session>;
}
```

`discover` + `stream` are the read path. A whole-session parse is not a fifth method:
`adapter.parse(&r)` is `stream::collect` over `stream`, so the streaming and the materializing
readers cannot diverge. The trait's other methods all have defaults: `try_stream`, `transcript`
(a read whole, from a verified checkpoint, or as an append-only file's past prefix), `dependencies` (the side files a read of
one session consults, so freshness can watch them), `store_affordances` (plans, goals and
schedules that live only in a side store) and `copied_before` (the turns a fork copied from its
source). The shared pieces an adapter imports instead of retyping live beside the trait:
`harness/wire/{anthropic,openai}` (the two content shapes most stores persist, read into blocks
and written back) and `harness/store` (`jsonl::Read`, the one line reader in both builds, and
`jsonl::drive`, the one transcript lifecycle over it; `json::validate_value`; `sqlite::Cells` and `open_ro`, which opens every database
read-only; `write_jsonl`, `xdg_data_dir`). Every family reads through one record codec over one
byte source; `docs/design/CODEC.md` is the rule.

Being a **conversion target** is not part of the trait either. An emitter is a free function,
`harness/<h>/emit.rs`'s `emit(&Session, &Path, &EmitOptions) -> Result<EmitResult>` — the
format's fields written by the shared encoder plus a store writer — registered in the one table
`emit::emitter_for`, from which `supported_targets()`, the `--harness` target list and the
round-trip test are derived. `EmitResult { path, new_id, resume_hint }` carries a human nudge
like `claude --resume <id>`.

The `Send + Sync` bound is load-bearing: it's what lets discovery fan out across adapters in parallel. Every adapter holds only path data (connections are opened per call), so the bound is always satisfied.

Adapters are registered in one place — `harness::for_harness(h)`, a match from each `Harness`
to its `detect()`ed adapter; `harness::all()` is that match applied to `ORDER`, the array of
every harness in listing order. `detect()` resolves each store from `$HOME` and the environment,
and a per-session lookup detects only the one harness it needs (detection stats roots and reads
config). The adapters whose primary store is SQLite (Cursor, Hermes, Goose, Zed, Devin) are behind
the `sqlite` feature so the WASM build can drop them. Adding a harness is "write one `Adapter`, add
an arm to `for_harness` and a line to the `harnesses!` list" — the
[adding a harness](adding-a-harness.md) chapter walks the whole thing.

## Discovery & the incremental cache

`discover_all()` is the front door. It collects every registered adapter with a root on this machine, then runs their `discover()` jobs **in parallel via rayon** (`jobs.into_par_iter().map(run_job)`), so the cost is the slowest single adapter rather than the sum. (Turn the `parallel` feature off — e.g. on `wasm32` — and you get an identical sequential pass. The file-heavy adapters use the same trick *internally* via `par_filter_map` / `par_flat_map` to scan many transcripts at once, and `cv index` runs discovery with a hand-off so a compressed Codex rollout decoded for discovery is read by the indexer beside it, never decoded twice.)

A failing adapter logs and contributes an empty list rather than sinking the whole scan — one harness's corrupt file never breaks discovery of the other twenty.

### The cache

Cold discovery of a large corpus means reading and JSON-parsing every transcript just to pull a handful of metadata fields. That's slow. But a transcript that has not changed has unchanged metadata, and an append-only log that only grew still has the metadata its old prefix produced.

That's the trick behind [`discover_cache.rs`](https://github.com/emberian/cv/blob/main/crates/cv-core/src/discover_cache.rs): the `scan_cache` table in the catalog database (`$CLUSTERVISION_HOME/catalog.db`), keyed by path, `(mtime_ns, size)`, the adapter's interpretation version and the captured file identity (`source::FileId`: length, mtime, device, inode, ctime), storing the `SessionRef`s each file produced. A hit returns the stored refs after checking captured identities and dependencies, without a full read+parse — and a `.jsonl` that only grew continues from its stored position, so a listing beside a live writer costs the appended bytes. Measured 2026-10-14 on a frozen copy of the maintainer's fleet (5,421 sessions, 78 GB): the first `cv ls` on an empty home took 158–186 s; a warm `cv ls` 0.06–0.09 s. The 0.17 binary lists the live fleet of 7,374 sessions in 2.36 trillion instructions on the first run and 0.6 billion warm (`docs/design/PERF.md`).

Adapters opt in by wrapping their per-file scan in `cached_scan_with_reads` (one session per
file, with the side files the scan consults captured first) or `cached_scan_many` (a file that
holds several sessions, as Gemini's logs do):

```rust
crate::discover_cache::cached_scan_with_reads(&transcript, reads, || scan(&transcript))
```

Some careful properties make it safe to lean on:

- **Correctness never depends on it.** An entry is reused only when the tuple, the interpretation version and the captured identity all match, so a rewrite with a restored timestamp misses and re-scans. A missing or corrupt cache just means everything is re-scanned (and the cache rebuilt).
- **It's scoped to the resolved environment** in process and persisted for `cv` and the desktop app through the catalog database; the same rows feed the freshness probe's append detection.
- **Empty results aren't cached** — a non-transcript path is cheap to re-reject.

### The head/tail cap

One more guard keeps discovery fast on pathological inputs. Some transcripts are *enormous* — Codex rollout logs can run to hundreds of megabytes. Fully parsing those during a *listing* would defeat the point of the cheap-`SessionRef` design.

So the file-based adapters cap how much they read during discovery. The Codex adapter is the clearest example: plain files up to a `FULL_SCAN_CAP` (8 MiB) are parsed in full; larger plain files are **sampled head + tail** (1 MiB each). The head carries `id` / `cwd` / `title` / `created_at`; the tail's last record carries the real `updated_at`. Codex discovery leaves `message_count` as `null` (not counted), including compressed `.jsonl.zst` rollouts; zero is reserved for a counted session with no turns. Compressed metadata still requires decoding the source. A full read or index pass certifies exact catalog counts.

### Sub-agents

Sub-agent sessions (each adapter's `forest`: Claude, Codex, Devin, Kimi, Kimi Code, Hermes) are **not** in the main pool — there can be thousands of them, so pulling them all into every listing would be wasteful. They're fetched lazily per parent via `subagent_tree_of(&SessionRef)`, which is what powers the sub-agent trees in the app.

### Finding one session

`find(id, harness)` is the lookup the CLI and MCP server use. An exact `id` match wins and returns immediately; otherwise the `id` is treated as a **prefix**. A single prefix hit is returned — but *multiple* distinct prefix hits are an **error**, not a silent "first one wins," so you're told to pass a longer id or a `--harness` to disambiguate. Surprising the user with the wrong session is worse than making them type three more characters.

## The crates

clustervision is a Rust workspace. `cv-core` is the library every other crate is built on; the rest are thin front-ends over it.

```text
                 ┌─────────────────────────────────────────────┐
                 │                  cv-core                     │
                 │  IR · Adapter registry · discover_all +      │
                 │  cache · emit/convert · loom · tasks · the   │
                 │  read API · render · ingest · redact · watch │
                 └─────────────────────────────────────────────┘
                     ▲              ▲              ▲          ▲
          ┌──────────┴─────┐   ┌────┴─────┐  ┌─────┴────┐ ┌───┴────┐
          │       cv       │   │cv-search │  │  cv-web  │ │  app/  │
          │ CLI · mcp ·    │──▶│ (tantivy,│  │  (WASM)  │ │(Tauri) │
          │ serve · watch  │   │ model2vec│  │          │ │        │
          └────────────────┘   └──────────┘  └──────────┘ └────────┘
```

| Crate | Responsibility |
|-------|----------------|
| **`cv`** | The one binary: the CLI (read, search, reshape, port), `cv mcp` (the MCP server, its tools generated from the CLI's own clap tree), `cv serve` (the HTTP read API, the task inbox, the dashboard), `cv watch` / `cv archive` (the central archive), and the optional LLM digests and `--generate` continuations (`llm.rs`, via OpenRouter / Anthropic / a local LM Studio). |
| **`cv-search`** | Pure-Rust search: full-text via [tantivy](https://github.com/quickwit-oss/tantivy) (BM25 + snippets) and semantic via embeddings (model2vec). |
| **`cv-web`** | WASM bindings — ingest an uploaded harness `.zip` entirely in-browser and hand back sessions as JSON. |
| **`app/`** | The desktop app — a Tauri v2 shell around the web UI, reading through `cv_core::api` in-process. |

The shape to take away: **`cv-core` owns the IR, the adapters, and discovery; everything else is a lens onto it.** A bug fixed in an adapter improves the CLI, the server, the web UI, and the app at once — because they all see the same `Session`.

## Where to go next

- [Cross-harness conversion](conversion.md) — the `emit` side of the trait, and what survives a round-trip.
- [Adding a harness](adding-a-harness.md) — write one `Adapter`, add one arm to `for_harness`, done.
