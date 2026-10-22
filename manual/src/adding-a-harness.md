# Adding a harness

Got a coding agent we don't support yet? An adapter is three things — where the harness keeps its
sessions, how to list them cheaply, and how to read one as a stream of messages. Its size is the
store description (60–150 lines, plus about 25 per SQL table), the format's genuine quirks (0–400),
the codec (none when the store writes the Anthropic or OpenAI content shape, which the shared wire
codecs read; 500–1,000 for a record grammar of its own), a few dozen lines of glue, and an emitter
if it is a port target (150–450). [Grok's](https://github.com/emberian/cv/blob/main/crates/cv-core/src/harness/grok.rs),
1,068 lines with its emitter, is the model to copy; Roo, 132 lines over Cline, is the smallest.
The steps:

1. **Name it.** One line in the `harnesses!` list in `crates/cv-core/src/ir.rs`
   (`Yours => "yours",`) gives the `Harness` variant, its canonical spelling on every door, and
   `Harness::ALL`. The same macro embeds `formats/yours.toml`, so step 5 is not optional: the
   crate does not compile without it.
2. **Write `harness/yours.rs`** with a struct holding the resolved root(s) and an `Adapter` impl:

   ```rust
   pub struct Yours { root: Option<PathBuf> }

   impl Yours {
       /// Resolve the store from `$HOME` / the env (existing paths only).
       pub fn detect() -> Self {
           Yours { root: crate::env::home_dir().map(|h| h.join(".yours/sessions")).filter(|p| p.exists()) }
       }
   }

   impl Adapter for Yours {
       fn harness(&self) -> Harness { Harness::Yours }
       fn roots(&self) -> Vec<PathBuf> { self.root.clone().into_iter().collect() }
       fn discover(&self) -> Result<Vec<SessionRef>> { /* walk the root, `scan` each file */ }
       fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
           /* `store::jsonl::for_each_line` over the file, one `record(view, demand)`, sink each Message */
       }
   }
   ```

   - **`roots`**: every directory you read from, the primary one first (an emitter writes there,
     the catalog watches each). For a SQLite store, the directory holding the database.
   - **`discover`**: one `SessionRef::new(id, Harness::Yours, path)` per session, with what is
     cheap to know filled in by struct update (`SessionRef { cwd, title, message_count,
     ..SessionRef::new(id, h, path) }`). `message_count` is user + assistant turns only.
   - **`stream`**: read records one at a time and hand each `Message` to the sink, so a 1 GB
     transcript costs one record of memory. Start the session with `Session::new(id, h)`; read
     lines with `store::jsonl::for_each_line` (each line validated once into a `JsonView`; a
     corrupt line never fails a session) and count the skips with `note_skipped_lines` (the skip
     is never silent). One `record(view, demand)` function decodes every record kind, resolving
     only what the demand asks for ([`docs/design/CODEC.md`](https://github.com/emberian/cv/blob/main/docs/design/CODEC.md));
     a SQLite store reads its cells through `store::sqlite::Cells`. A store that can only be read
     whole parses it and calls `stream::replay`.
   - There is no `parse`: `adapter.parse(&r)` is `stream::collect` over your `stream`, so the
     whole-session and streaming readers cannot diverge.

3. **Map content through the wire codec, don't re-type it.** If your store writes the Anthropic
   content shape (`text | thinking | tool_use | tool_result | image | document`), call
   `wire::anthropic::blocks_from(content, &anthropic::ANTHROPIC)`; if it writes OpenAI parts and
   `tool_calls[].function.arguments` as a JSON string, `wire::openai::blocks_from` and
   `openai::tool_calls`. A store that renamed a key (OpenClaw's `toolCall`, Goose's
   `redactedThinking`) gets a `Quirks` constant beside the others in `harness/wire/`, not a copy of
   the mapping. `wire::coerce_text` / `text_parts` flatten a string-or-parts field. A SQLite store
   opens through `store::sqlite::open_ro` (read-only, never a lock on a live database) and probes
   its schema with `columns` / `table_exists`; an XDG store finds itself with `store::xdg_data_dir`.
4. **Register it**: one arm in `harness::for_harness` and one entry in `ORDER` (`harness/mod.rs`).
5. **Add `formats/yours.toml`** (below).
6. *(Optional)* **Make it a port target**: `harness/yours/emit.rs` with
   `pub fn emit(&Session, &Path, &EmitOptions) -> Result<EmitResult>` — the codec run backwards
   (`anthropic::assistant_blocks`, …) plus a store writer — re-exported as `yours::emit` and
   added to `emitter_for` in `emit.rs`. That one arm is the whole registration: `supported_targets()`, the
   `--harness` target list and `round_trip_all_supported_targets` are derived from it, so the round trip runs the moment
   the arm exists. Tell `target_holds` in `emit/fidelity.rs` what your store can carry, so `port --strict`
   knows an expected loss from an unexpected one.

## Three rules the IR asks of you

These are what keep an adapter from becoming a pile of special cases every other module has to know
about. All three are checked by tests, not just convention.

**1. Set `kind` and `origin` on every message.** `role` says *who* speaks; `kind` says *what the
turn is* and `origin` says *where it came from*. `Message::new(role)` fills in a default
(User→`Prompt`/`Human`, Assistant→`Reply`/`Model`, Tool→`ToolResult`/`Harness`,
System→`Notice`/`Harness`) so the type is always inhabited — but leaving the default in place where
the store actually told you more is a bug. A system reminder the harness injected is
`MessageKind::InjectedContext`, not `Notice`. A compaction seam is `CompactionBoundary`, not a flag
in your `extra` bag. A hook's stdout is `Origin::Hook`. Consumers — `cv doctor`, `compaction.rs`,
the dataset exporter, every emitter — read `kind`, and they will never look for your harness's
private field.

**2. Put harness-specific facts in the harness bag.** Use
`msg.harness_extra_mut(Harness::Yours)` / `session.harness_extra_mut(Harness::Yours)`, which hand
you the `extra["<harness>"]` object. Never write a flat key at the top level of `extra`. Keep the
harness's own spelling for the keys inside (`attachment_type`, `history_mode`); where you invent a
name, make it snake_case.

```rust
// yes
msg.harness_extra_mut(Harness::Yours).insert("thread_state".into(), json!(state));

// no — a flat key every other harness's code now has to not-collide with
msg.extra.insert("thread_state".into(), json!(state));
```

Two top-level keys are not harness names, both on a message and both cv's own streaming
bookkeeping: `_record`, the verbatim source record carried under `ParseOptions::complete`, and
`cv_byte_offset`, stamped on the lazy-offset path. Facts cv produces itself rather than reading
from your store — a parse diagnostic, provenance for a session cv synthesized — go under
`extra["cv"]` via `Session::cv_extra_mut`. A session's `extra` has no flat keys at all, and
`harness::assert_no_flat_keys` in your tests will hold you to it.

**3. Shared concepts get their first-class home, not a bag entry.** Before you reach for `extra`,
check whether the IR already has the field: the system prompt is `Session::system_prompt`; fork,
parent, continuation and sub-agent pointers are `Session::lineage`; structured tool-result state is
`Block::ToolResult::details`; token counts and cost are `Usage`; an unanswered API error is
`MessageKind::Error`. Something in `extra` that another harness also has is a sign the IR is missing
a field — say so rather than burying it.

## The format manifest

Every adapter is paired with `formats/<harness>.toml`. It records what the source cannot say: the
upstream it was verified against, where the store lives, and the judgments about vocabulary the
adapter does *not* match on by name:

```toml
harness = "yours"
source_files = ["harness/yours.rs", "harness/wire/anthropic.rs", "harness/wire/mod.rs"]
ignore_literals = ["jsonl"]
store = ["~/.yours/sessions/<id>.jsonl"]

[upstream]
repo = "https://github.com/someone/yours"
commit = "a1b2c3d"
date = "2026-09-19"

[types]
"record.checkpoint" = { status = "carried", note = "replayed on same-harness port" }
"record.telemetry" = { status = "ignored", note = "UI-only" }
"system.away_summary" = { status = "generic", note = "the `_` arm makes every other subtype a notice" }
"table.events" = { status = "handled", note = "a SQL identifier, not a match arm" }
```

A type your adapter has a match arm for (`"user" =>`, `Some("summary")`) is `handled` by
inference — the arm is the spec, so the manifest does not repeat it. The statuses you write are
`generic` (interpreted by a blanket rule, so no per-item literal exists), `carried` (kept verbatim
as a carrier message under `ParseOptions::complete`), `ignored` (known and deliberately skipped),
and `handled` only for something no match arm names (a SQL table or column, a store path, a tool
name). `ignore_literals` lists match-arm literals that are not vocabulary (file extensions, a
role word the manifest lists elsewhere). Name the wire codec you read through in `source_files`.

`cargo test -p clustervision-core --test formats_manifest` holds the manifest to the source: a
listed `handled` type must appear in the adapter (or its codec), no `handled` entry may restate a
match arm, and every `ignore_literals` entry must still be an arm literal. [`cv formats`](cli.md#cv-formats)
then parses real sessions in complete mode and tells you what the harness is writing that your
manifest has never heard of — which is how you find out upstream shipped a new record type.

## Send us your transcripts

The fastest way to get it right: **send us your transcripts.** We can only test what we can see, and
historical format variants are an explicit goal. Open an issue or a PR with a few real (redacted, if
you like — `cv export --redact` helps) session files. 💜

Run `python3 -I tools/check-size.py` to review product/test line drift against the last release
tag. It reports drift without line targets or percentage ratchets; design documents stay under
250 lines. The old `--write` option and committed `tools/size-budget.txt` are removed.
