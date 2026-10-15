# One codec per wire format

"Never materialize" was built as extra decoders beside each adapter's owned one (Claude had
five: owned Value, typed borrowed, positioned "portable", lean, facts), plus parity tests that
those paths agree. The rule now: **a wire format has one decoder, parameterized by demand; a
build differs only in its byte source.** Every family is on it as of 0.17.

**Byte source.** `store::jsonl::for_each_line(source, file, from, to, visit)` yields each
physical line once as a `Line { at, end, complete, view }`, with `view: Option<JsonView>` (None
for blank, non-UTF-8 or rejected lines). Mapped builds borrow lines from one mapping; positioned
builds (and a failed mapping) read lines up to 64 KiB into one buffer and leave a giant line in
place as a positioned view. That `cfg` is the only one a reader needs; `JsonView` has the same
API over borrowed, positioned and owned (`(&Value).into()`, for strings with no file) values.
Each line is validated once by `store::json::validate_value` (exactly serde `Value` acceptance,
property-tested against serde), which also indexes long strings and containers so every later
field selection skips them in O(1). Lines are ASCII-trimmed, as JSON allows.

**Decoder.** One function per format, `record(view, demand) -> session facts + Option<Message>`
(Claude: `harness/claude/record.rs`). It selects the fields any record kind reads in one pass
(`Fields::parse`), then resolves only what the demand asks for: `ParseOptions` plus the sink's
`extra_projection`. Content goes through the family's block codec (`wire::anthropic::
block_from_view_with`) with the shared text rule `JsonView::lazy_text(spans)`: decoded length
over `INLINE_MAX` stays in the source (a `Span` when mapped, a checked `Text::Source` when
positioned), tool arguments over `INLINE_MAX` become `Json::Source`. Lean, facts, usage, rich and
complete are which fields get resolved, never which code runs. Discovery, fork heads and
prefix counts are demands on the same reader. The driver rechecks source identity after a
record that published a source descriptor and once at the end, never per record.

**Heap budgets by construction.** Nothing unselected is decoded, so a reducer's heap is the
fields it reads; a giant line is never copied on positioned builds; spans and source arguments
defer payloads to the consumer. The CI heap gate (`check-heap-budget.py`, both builds) and the
operation gates stay the proof. Still owned, and so still output-sized: complete-mode carriers
and metadata, rich `toolUseResult` sidecars, array tool results under 64 KiB (coerced inline).

**Tests.** Per format: the corpus golden summaries (frozen per harness version) and the heap
gate, plus behavior tests where the rule lives (classification, checkpoints and resume,
source-change rejection, offset stamps). Shared primitives (`validate_value`, the streaming
classifiers) are tested against simple oracles. No test compares two decoders: there are none.
A port is checked once, before deleting the old paths, with a scratch dump of every message
(span descriptors included), checkpoint and session over real sessions in every demand,
byte-identical before and after; that tool is not committed.

**Byte sources by family.** JSONL (Claude, the OpenAI family, Gemini's jsonl era): `for_each_line`.
JSON documents (Gemini's document era, cline, roo, cline_sdk, lmstudio, opensession, the exports):
`JsonSource::read_document`/`read_archive`, with an owned view (`(&Value).into()`) for string entry
points. SQLite stores (opencode, cursor, devin, goose, zed, openclaw, hermes): `store::sqlite::Cells`,
each cell copied once into the source's arena, validated once, then viewed. `codex`, `cursor` and
`goose` `legacy` modules are readers of older formats, not oracles. Test-only legacy emitters are
retired to frozen goldens (`tests/fixtures/emit-frozen/`, `Frozen` in `emit/grok_qwen_tests.rs`).
A change whose dump differs must explain each difference in its commit; the ports' are in git.
