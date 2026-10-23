# One codec per wire format

"Never materialize" was built as extra decoders beside each adapter's owned one (Claude had
five: owned Value, typed borrowed, positioned "portable", lean, facts), plus parity tests that
those paths agree. The rule now: **a wire format has one decoder, parameterized by demand; a
build differs only in its byte source.** Discovery must share that classification too; remaining
count-only scans are tracked in [`DATAPATHS.md`](DATAPATHS.md).

**Byte source.** A captured `store::jsonl::Read` (`lines`, `prefix`, `snapshot`) yields each
physical line once as a `Line { at, end, complete, view }`, with `view: Option<JsonView>` (None
for blank, non-UTF-8 or rejected lines). Mapped builds borrow lines from successive 8 MiB windows (grown for a longer line); positioned
builds (and a failed mapping) read lines up to 64 KiB into one buffer and leave a giant line in
place as a positioned view. That `cfg` is the only one a reader needs; `JsonView` has the same
API with two arms: a source view over `RangeSource` (mapped/borrowed bytes or positioned reads),
and an owned `Value` view (`(&Value).into()`, for strings with no file).
Each line is validated once by `store::json::validate_value` (exactly serde `Value` acceptance,
property-tested against serde). A bounded endpoint index lets cached long strings and containers
be skipped in O(1); an evicted endpoint can require rescanning. Lines are trimmed of JSON whitespace (space, tab, LF, CR) only.

**Decoder.** One function per format, `record(view, demand) -> session facts + Option<Message>`
(Claude: `harness/claude/record.rs`). It selects the fields any record kind reads in one pass
(`Fields::parse`), then resolves only what the demand asks for: `stream::Demand { tier, spans,
offsets }`, tiers ordered Discovery < Lean < Usage < Facts < Rich < Complete, the sink's `tier()`
narrowing Rich to Facts per message (`Demand::for_sink`); an adapter's own codec demand is built
from it once per read. Content goes through the family's block codec (`wire::anthropic::
block_from_view_with`) with the shared text rule `JsonView::lazy_text(spans)`: decoded length
over `INLINE_MAX` stays in the source as checked `Text::Source` (a `JsonRange` of the read's own
capture), in the primary transcript as in independent files, cells and decoded backing; `Span`
is only a serialized wire form, resolved through the same checked range. Large tool arguments
become `Json::Source`. Lean, facts, usage, rich and
complete are which fields get resolved, never which code runs. Discovery, fork heads and
prefix counts are demands on the same reader.

**One lifecycle.** A JSONL transcript is `Adapter::transcript(r, demand, Start, sink)`
(`Whole`, `Checkpoint`, `Prefix(len)`), read by `jsonl::drive(read, from, codec, sink)`. The
adapter owns only its `Codec { record, settled, demand, session, state, finish, VERSION }`;
the driver owns the rest: declining a start past EOF or a checkpoint of another codec version
or demand (`jsonl::restore`), counting every rejected nonblank line (no codec opt-out),
offering a checkpoint after every
finished line the codec is settled on, the guard, and the final check after held output. Kimi's
rotated segments keep their own loop (its checkpoint sits at a line's start) but use the same
guard and stamp. Who checks the backing, and when, has one answer each: `stream::Guard` after a
callback slower than `SLOW_CALLBACK`, after the session metadata and after a checkpoint; the
line pass once per 8 MiB window; `Read::check` at EOF and guard completion (including failed callbacks). No reader calls `check_backing`.
The guard rechecks source identity after slow callbacks and at completion; the identity is
`source::FileId`, and a `.jsonl` that grew under the read is admitted by content
(`FileId::admits`: same inode, at least as long, sampled prefix unchanged), with the trailing
partial line held back. Every lazy text a reader hands out resolves through its checked capture,
so a replaced or shrunk file fails the first content read instead of returning the replacement's
bytes. Every reader follows the `stream` module's ownership rule: the descriptor
is checked after a slow callback (`SLOW_CALLBACK`), once per window of mapped input, at a
checkpoint and at EOF; a source other than the current file (Kimi's older segment, its wire
enrichment) is checked against its own capture the same way. Completion rechecks the pathname.
A slow callback that truncates the file is rejected before the mapping is touched again; a fast
one, and any concurrent external write or truncation, can still fault a mapped read: mappings
are not immutable snapshots.

**Heap budgets by construction.** Materialization follows demand, while validation can still
traverse unselected bytes. A reducer retains the fields it reads; a giant line is never copied on positioned builds; spans and source arguments
defer payloads to the consumer. The CI heap gate (`check-heap-budget.py`, both builds) and the
operation gates stay the proof. Still owned, and so still output-sized: complete-mode carriers
and metadata, rich `toolUseResult` sidecars, array tool results under 64 KiB (coerced inline).

**Tests.** Per format: the corpus golden summaries (frozen per harness version) and the heap
gate, plus behavior tests where the rule lives (classification, checkpoints and resume,
source-change rejection, offset stamps). Shared primitives (`validate_value`, the streaming
classifiers) are tested against simple oracles. No test compares two decoders: there are none.
Before deleting an old path, compare hashes of canonical outputs, resolved payloads, checkpoints
and session metadata against an independent baseline over recorded sessions in each affected
demand. Stream hashes rather than retaining transcript dumps; diagnostic tools stay uncommitted.
The byte-identity gate is `tools/dump-hash.sh --cv BIN --probe store_probe [--probe-positioned
store_probe] --scratch DIR [--home DIR]`: over `tools/dump-hash.manifest` (every corpus version
staged as a fake HOME, four synthetic Claude edges, any external home of real sessions) it prints
one sorted `sha256  command  session  demand` line per `ls`/`show`/`export`/`search`/`events`/
`tools`/`stats`/`doctor`/`pack` form, per reshape output and sidecar, and per `store_probe hash`
demand in both builds. Two runs of one binary print equal sets whatever `--scratch` is called (the path is masked); a commit that changes output
pastes the lines it moved into its message. Results are never committed.

**Byte sources by family.** JSONL (Claude, the OpenAI family, Gemini's jsonl era): `jsonl::Read`.
JSON documents (Gemini's document era, cline, roo, cline_sdk, lmstudio, opensession, the exports):
`JsonSource::read(ReadInput, Validation, callback)`, with an owned view (`(&Value).into()`) for string entry
points. SQLite stores (opencode, cursor, devin, goose, zed, openclaw, hermes): `store::sqlite::Cells`,
each cell copied once into the source's arena, validated once, then viewed. `codex`, `cursor` and
`goose` `legacy` modules are readers of older formats, not oracles. Test-only legacy emitters are
retired to frozen goldens (`tests/fixtures/emit-frozen/`, `Frozen` in `emit/grok_qwen_tests.rs`).
A change whose output differs must explain each difference in its commit.

**Writing shared wire formats.** Anthropic blocks use one grammar with explicit leaf publication:
checked streams resolve source text; the public owned API preserves Span descriptors and resolves
Source text. Only callers requesting an owned Value buffer that value. Qwen and Gemini share
GenAI part selection and text/media fields, preserving their native malformed-field qualification;
consumer-specific tool IDs, wrappers, signatures and demand remain in the adapters. Independent
frozen outputs check both contracts without retaining a second production codec.

**Store selections.** `store::sqlite::read` brackets an operation with the existing descriptor
and native read-set checks. Private `Selection` databases hold derived membership, never write
the native store, and belong to its operation inventory. OpenClaw promotes from native SQL
after another query cache miss; Hermes indexes normalized digests and confirms collisions by
exact source comparison. Native ordering, affinities and collations remain authoritative.
Portable `selection::Positions` indexes Qwen task times and export IDs without copying payloads.
Goose retains its already indexed scalar family selection. Page-cache settings and inventory
admission charges do not bound individual SQLite cells or demanded candidate vectors.

**Bounded projections and temporary bytes.** `projection::Head` owns both explicit budgets: an
exact UTF-8 byte prefix for search, or a character label with trim/ellipsis policy for titles.
Unresolved `Text` refuses `Display`; inline-only comparison returns `Option<bool>`. Markdown,
HTML and redaction take the caller's resolver. Fidelity has only the streaming accumulator,
checked against frozen feature counts. Arena copies and quoted JSON use one bounded scratch
writer; exact-string storage works without SQLite. The independent `zstd` feature selects the
decoder; a compressed read without it fails instead of interpreting compressed bytes as JSONL.

**Distinct contracts are not decoder twins.** JSON framing trims only JSON's four ASCII
whitespace bytes; user text and labels classify Unicode whitespace. Neither can substitute for
the other. `spans` changes payload ownership, not which metadata exists: Zed's owned `Value`
extras therefore remain owned in a span read. Suppressing them requires a metadata tier, not a
span-dependent grammar. OpenSession validates message shapes before the first callback so a
malformed later message cannot publish a successful prefix; its publication pass has a different
purpose from that validation. Emitters preflight before creating output and check once before
commit; those checks protect different publication boundaries.
