# Store probe

This diagnostic example performs one operation per process without modifying harness stores.
It supports every harness (the SQLite ones with SQLite enabled) through the registry's
explicit-root constructor, `harness::for_harness_with_root`. Direct stream operations never update
the catalog or detect installed homes to resolve IDs. The `sessions` operation follows production
fleet detection from `HOME`/XDG and may perform a scoped or full discovery.

The composed `sessions` and `affordance-read` operations may update the catalog; `discover`
initializes its scan cache. These require an existing scratch `CLUSTERVISION_HOME` and refuse
the real catalog directory. Claude fork detection and adapter side-store reads still examine
siblings of the supplied source, so use a frozen scratch directory. `discover` scans `--root`,
`sessions` uses isolated `HOME`/XDG roots, and the remaining operations read `PATH`.

Build clean timing and requested Rust heap versions separately, with your own target directory:

```sh
CARGO_TARGET_DIR=/tmp/my-probe-target CARGO_BUILD_JOBS=2 cargo build --release \
  -p clustervision-core --example store_probe
cp /tmp/my-probe-target/release/examples/store_probe /tmp/my-probe-clean
CARGO_TARGET_DIR=/tmp/my-probe-target CARGO_BUILD_JOBS=2 cargo build --release \
  -p clustervision-core --example store_probe --features probe-heap
cp /tmp/my-probe-target/release/examples/store_probe /tmp/my-probe-heap
```

Run `OP HARNESS ID PATH`, optionally `--root DIR`, `--mode lazy|lazy-extra|facts|bulk|full|complete` and
`--materialize`; `--mode lazy-extra` retains full extras alongside lazy content.
`--mode facts` requests the shared reducer metadata projection; adapters without a projection
implementation retain full extras. Compare reducer outputs rather than Message JSON across these
two modes. The default mode is lazy. The root defaults to the input's parent; for discovery
pass the root explicitly: what `for_harness_with_root` documents for the harness (Claude's config
dir, Codex's home, Continue's global dir, LM Studio's conversations dir, OpenClaw's state dir, a
user home for Cline and Roo, the dir holding `sessions.db`/`threads.db` for Devin/Zed, a Cursor
`User` dir or CLI home, …). Directory session
paths (Kimi, Kimi Code and Grok) report directory metadata size, not total transcript/sidecar bytes;
use the generator manifest for total generated physical source sizes.
OpenSession and account exports scan only the explicit root, excluding configured exports and
`CV_EXPORTS`; their direct stream operations select the supplied ID from the supplied document.

| Operation | Work measured |
|---|---|
| `baseline` | Adapter/input setup has completed; empty stage |
| `discard` | Direct stream, count callbacks, immediately discard every message |
| `count` | Same stream plus transcript/session/store affordance counts |
| `checkpoint` | Count through the resumable pipeline; return last checkpoint in `detail.checkpoint`, every `--chunk-bytes N` bytes |
| `resume` | Continue fixture checkpoint from `--resume-from FILE`; report suffix callbacks and whole-session counts |
| `doctor` | Production context attribution report, using the requested parse mode |
| `token-stats` | Production token statistics for the explicit session and its subagent forest; scratch catalog required |
| `dataset` | Production staged ChatML export; optional `--query EXPR`, scratch catalog required |
| `dataset-collect` | Former lazy collection plus ChatML writer; same filters and scratch requirement |
| `collect` | Direct stream into a retained message vector; optional materialization |
| `window` | Stream to EOF, retain only `--last N` messages (default 20), optionally materialize |
| `affordances` | Full collection and materialization, classify and retain affordance rows |
| `affordance-read` | Production `affordance::read`; requires scratch `CLUSTERVISION_HOME` |
| `hash` | Resolve each message, sort owned JSON maps recursively, hash streamed JSON and final metadata |
| `discover` | Explicit-root discovery; requires scratch `CLUSTERVISION_HOME` |
| `sessions` | Production catalog listing/scoped refresh; requires scratch catalog and synthetic `HOME`/XDG roots |
| `chunks` | Direct span decoding; `--span OFFSET:LENGTH [--escaped] --chunk-bytes N` |
| `prefix` | Retain a text prefix under `--prefix-bytes N` raw encoded bytes; same span flags |
| `resolve` | Direct span materialization into a retained owned string; `--span OFFSET:LENGTH [--escaped]` |
| `resolved-len` | Direct decoded byte counting without owning text; same span flags |

`dataset` and `dataset-collect` render the same newline-terminated ChatML record to a counting
discard writer; `--hash-output` adds its SHA-256 digest. Both use lazy parsing regardless of
`--mode`; `--query` uses empty indexed text membership. The collected baseline retains its
session through the heap snapshot. The staged consumer retains no transcript messages and
reports callback counts as unobserved. Private staged bytes are included in its measured work.

`token-stats` calls `TokenStats::compute` on the explicit session reference. Its parse policy is
production-defined; `--mode` is ignored. The complete statistics are in `detail`; `messages`
stays zero because the operation does not expose callback counts. Child detection can examine
sibling files, so isolate the entire supplied source directory. Aggregation and JSON report
construction are both included in the stage.

For append diagnostics, save `checkpoint`'s `detail.checkpoint` as JSON, append to the same
scratch transcript, then run `resume --resume-from FILE`. These operations never save marks or
counts in the catalog. The diagnostic calls the adapter directly: **it does not validate inode,
sampled prefix, prior consumer output or dependency freshness**. Use only an unchanged fixture
prefix and matching parse mode. Production FTS performs those checks before reuse; the composed
regression tests compare search documents, events and counts with a whole read. `messages` counts
only callbacks in the resumed suffix, while `detail.turns` and `detail.counts` include the prefix.
A partial EOF record is replayed from the last complete safe checkpoint.

`--hash-output` explicitly enables consumer fidelity digests. Discovery hashes all returned
`SessionRef` metadata, sorted by ID/path (paths included); both affordance operations hash their
ordered rows, including summaries/items and store rows. Leave it off for throughput and allocation
measurements; compare output digests in separate fidelity runs.
`--warm-discover` runs the explicit-root adapter's discovery during setup and drops its returned
refs before timing/heap capture. It requires a scratch catalog. The report labels this setup and
includes retained cache metadata in baseline live heap; released source buffers are excluded
from the warm selected-read stage. Compare cold and warm separately, especially for no-mmap
account archives where cold admission can own the archive but warm reads own only a selected range.
For `resolve`, `--hash-output` adds a SHA-256 digest of the decoded UTF-8 bytes; the default
reports only decoded length. The owned result remains live through the heap snapshot, so its
requested output size is part of the measured heap and is not subject to the chunk scratch bound.

`json-len`, `json-write` and `json-prefix` exercise source-backed `Json` directly with `--span OFFSET:LENGTH`.
The range contains one complete JSON value, including string quotes or container delimiters;
`--escaped` is unused. All include source construction and strict validation in the measured
stage. `json-len` reports the canonical encoded byte count; `json-write` writes canonical bytes
to a counting discard sink, or with `--hash-output` to a streaming SHA-256 sink. Neither retains
an owned output or invokes the adapter. `json-prefix --prefix-bytes N` retains a canonical prefix
through the heap snapshot, reporting its byte count and truncation; the default budget is 64 KiB
and zero is allowed. `--hash-output` additionally hashes only the retained prefix. Compare hashes in separate fidelity runs so hashing
does not affect ordinary CPU or heap measurements.

`hash` emits independent SHA-256 digests of newline-delimited canonical message JSON and final
session metadata. It resolves spans and sorts input/details/extra maps in place, avoiding a second
whole-message Value tree or serialized string. It excludes session `source_path` (transport
location) only; all other serialized metadata remains. Compare **the same mode** before/after;
full/complete extras and carriers intentionally differ from lean modes. Hashing materializes one
message and must not be included in throughput/heap claims for a discard/count consumer.
`hash --hash-detail` also emits each message's fingerprint/serialized size and canonical session
metadata, to locate a mismatch without dumping all message text. It performs extra serialization
and retains fingerprint rows, so use it only for fidelity diagnosis. The production
`affordance-read` reports its parse/materialization policy as production-defined/unknown because
the same diagnostic must describe baseline and candidate library implementations truthfully.

The heap build reports successful Rust allocation requests, baseline/peak/end live requested
bytes and cumulative requested bytes. Its hooks include realloc requests and run throughout
process startup; stage boundaries never reset live bytes. Collected sessions/affordance rows
remain alive at the end snapshot. This excludes native allocations, mmap, stacks, allocator
overhead and fragmentation. Hook atomics affect timings: use the clean build for timing.

Use `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux) around each clean process for total
wall/user/system CPU, RSS and faults. JSON `wall_seconds` measures the stage only; process RSS
includes startup and setup. For macOS file-backed versus private residency also inspect
`vmmap --summary`/`footprint` on a long enough run. A directory/whole DB size is not selected
session bytes; this tool labels metadata size `physical_input_bytes` and does not report traversed
bytes (it cannot measure them). It never primes input with a whole-file read. Record explicitly warmed versus
unprimed input and catalog state, and do not claim a cold OS cache without controlling it.

Generate fresh representative fixtures outside the measured process:

```sh
python3 tools/perf-fixture.py gemini /tmp/my-gemini-fixture \
  --payload-bytes 100000000 --records 1 --escaped
```

`PERF-FIXTURE.json` records the explicit root/path/id, expected message count, decoded payload
bytes, encoded physical bytes and shape. The generator refuses an existing destination; it writes
unique IDs and parent links rather than repeating identical records. Use many records with the
same total payload to distinguish per-record overhead from giant fields, then repeat for Claude
and Codex. It creates synthetic scratch inputs, not new committed harness corpus versions.
The generator also supports Continue (unindexed history) and LM Studio (two versions per reply,
selected version first) for discovery scaling. Use fixed record size while increasing total bytes;
escaped discarded strings may still require scratch proportional to the largest field.
The native-format generator also supports `cline-sdk`, `opensession`, `chatgpt-export`,
`claude-export` and `kimi`. `--shape history` scales the selected transcript; account exports'
`--shape unrelated` puts a growing unrelated conversation before a tiny selected conversation.
Kimi's `--shape sidecar-unmatched` grows unmatched wire results and extras while preserving a
tiny selected last-wins result; `sidecar-selected` grows the output actually requested instead.
Manifests distinguish total generated payload, selected payload and each physical input file.
These cases derive their envelopes from corpus schemas, but are synthetic fixtures and do not
replace or regenerate corpus versions. Use `--records TOTAL_BYTES/262144` for fixed-size fields.
`zed` uses the corpus thread schema and the native level-3 zstd codec (the generator needs `zstd`
on PATH). Its manifest separates decoded JSON size, compressed blob size and SQLite file size;
generation and compression run outside the measured process.

For standalone escaped decoder allocation testing, `chunks` bypasses the parser entirely. A
file containing `x\\n` repeated N times is a valid raw JSON string body: use offset 0, length 3N,
`--escaped` and a small chunk. Build with `--no-default-features --features probe-heap` to measure
the positioned-read fallback separately from mmap. Check decoded bytes and maximum emitted chunk
alongside allocation growth; output hashing is a different experiment.

CI's live Rust heap gate is `tools/check-heap-budget.py MMAP_HEAP_PROBE --no-mmap-probe RANGED_HEAP_PROBE`.
It validates the binaries' reported build features and rejects missing allocation counters. The
16/64 MiB scratch cases check lazy Gemini/Claude/Codex giant fields and Claude/Codex stores with
fixed-size records in mmap builds. Valid/malformed escaped spans run with both mmap and positioned
reads; this no-mmap guarantee applies to the resolver, not whole-record adapter buffering. It checks expected message
or decoded byte counts, absolute heap ceilings and growth, without a wall-time threshold. Full
collection, deliberately owned tool arguments and native SQLite heap are outside these promises.
`--only stores` / `--only chunks` supports a scoped local check. CI's exact build commands are in
`.github/workflows/ci.yml`; the driver always uses a temporary catalog and deletes only its own inputs.

CI's separate native `--only active` pass extends the same 8 MiB absolute / 2 MiB growth budgets to discarded
fixed-size Cline SDK/OpenSession history, account archives' unrelated conversations, and unmatched
Kimi wire bodies. Every 16/64 MiB discovery probe gets a fresh catalog, preventing cache hits from
masking scan work. `--format FORMAT` narrows this campaign; `--baseline-probe HEAP_PROBE` also checks
small resolved-message/metadata and fresh discovery digests in separate fidelity runs. Selected
giant outputs and whole selected ChatGPT mappings are not included in these bounded claims.

`--only metadata-facts` runs actual doctor in Facts mode on synthetic Claude and Cursor CLI
unused metadata at 16/64 MiB, with plain and escaped payloads. Visible content and demanded facts
stay fixed; every full report must match a separate full-mode process and remain identical across
sizes. A 1 MiB heap cap and 256 KiB growth allowance reject rich-sidecar allocation. Fixture
`--shape metadata` reports decoded metadata payload separately from physical JSON/SQLite bytes.
The existing doctor gate includes these checks. Full references intentionally allocate demanded
metadata outside the Facts budget. These are native Rust heap promises, excluding SQLite/native
allocations, mapped pages and generation; no throughput or portable Cursor bound is claimed.

`--only doctor` checks giant Claude/Codex/Gemini decoded attribution in Facts mode against
separate full-mode processes, optionally using the supplied old baseline. Default CI's suite
includes stores/chunks, the five native active formats and the composed doctor pass. The doctor
gate compares Facts with the full report oracle in separate processes. Portable resolver promises are checked separately. CI also runs `--only portable-docs`:
Cline SDK, OpenSession, and both account archives have a distinct 2 MiB heap ceiling and
256 KiB growth allowance on the same 16/64 MiB plain/escaped cold inputs. Each discovery uses
a fresh catalog. Separate small lazy/full and discovery digests compare portable output with
the native reader. This is a positioned-source payload bound, not a bound on arbitrary selected
output or archive/session cardinality.
`--only active --format zed` explicitly opts into compressed-thread discovery/streaming with
fixed-size messages, retaining the same budgets; native SQLite allocations remain excluded.

`query --query EXPR` invokes the production predicate reducer; `query-collect --query EXPR`
uses the independent lazy collector followed by `matches_full`. Both require a scratch catalog,
report the same boolean result, and force lazy parsing. The collected session stays alive through
the heap snapshot. Use rich tool arguments and repeated events to measure retained history;
large text spans alone do not exercise that distinction. Callback counts are not observed by
these composed APIs (`messages` is null). Compaction predicates have a separate validation pass and should be
measured separately from ordinary tool/model/path queries.

`perf-fixture.py claude|codex NEW_DIR --shape tool-arguments --records N --payload-bytes B`
generates N Edit calls with distinct absolute paths and model `test`; B is the total decoded
argument content. Fixed-size arguments across larger N expose query retention. Setting B to
zero and growing N isolates event/histogram retention without a demanded giant argument.

`check-heap-budget.py ... --only query` compares the production reducer and collector on
16/64 MiB Claude/Codex histories with fixed 256 KiB arguments. Queries demand the last unique
target, verify the boolean oracle, and enforce the same 8 MiB / 2 MiB native heap limits.
The collector's peak is reported as a negative control, without applying the reducer budget
to its deliberately retained IR. A single giant argument still requires a record-sized Value;
this gate promises bounded history retention, not lazy argument materialization.

`tools` streams through the production `ToolHistogram::from_stream` used by forest tools;
`tools-collect` uses the previous EventSink collection followed by `from_events`. Both force
lazy parsing and report the complete small histogram as `detail`, so exact output parity needs
no additional digest or transcript materialization. Neither updates the catalog. Callback counts
are not observed by the composed histogram API (`messages` is null). Use fixed-size tool calls with distinct targets
and one tool name to distinguish retained event history from demanded histogram output. The old
path includes EventSink's per-record checkpoint requests; the histogram consumer never resumes
from or stores them.

The event operations include extraction and SQL publication. `events` reports the logical temporary
file bytes written and read once, excluding filesystem block rounding and catalog IO; small batches
stay in memory. Temporary files are exclusively created with private permissions and removed on
success or ordinary errors. A process crash can leave its private temporary file for OS cleanup,
but the catalog transaction rolls back. Distinct targets are capped in the publication cache, and
ordered batches retain deduplication keys only for one message. A single enormous message or target
remains a largest-record cost. Compare fixed-size records at increasing message counts; separate
heap and clean timing runs, and state whether the scratch catalog is new or already populated.

`tools/check-heap-budget.py MMAP_HEAP_PROBE --no-mmap-probe RANGED_HEAP_PROBE --only events`
is the event retention gate run in CI. It generates 20,000 and 200,000
calls per case: unfinished Claude Edits, open-many/close-in-reverse/reopen, and Codex visible patches
with native completions adding another path. Both mmap and positioned probes (with `sqlite`) verify
every persisted row and source stamp, temporary-file cleanup and I/O accounting, with unchanged
1 MiB Rust heap and 256 KiB growth limits. The retained `events-collected` control must produce
identical rows and demonstrate growing heap. Native SQLite memory, giant native diffs/tool
arguments and crash cleanup remain outside this heap guarantee.

`hash --raw-order` resolves content but preserves IR map insertion order in message and metadata
digests. It detects serialization-order drift that the default canonical digest deliberately
normalizes. It does not compare original harness JSON bytes or whitespace. The report marks
canonical JSON version as null and serialization order as preserved; ordinary hash output stays
compatible with canonical version 1. Use the same switch on both compared binaries.

CI also runs `--only active --format zed` with zstd installed for generated compressed rowid fixtures. This caps requested Rust heap; separate process-footprint measurements cover SQLite native memory.

SQLite synthetic scale inputs are available through `tools/perf-fixture.py`: `devin`
(history, metadata, unselected branches), `hermes` (history, prompt-only), `goose`
(history), `opencode` (history, many parts in one message), and `cursor` (external
bubbles or inline conversation). These write a new explicit directory and preserve
existing corpus versions. `--records` controls row count separately from total
`--payload-bytes`; manifests report both logical payload and physical database size.
Devin metadata cases isolate session-wide navigation retention, while prompt-only
Hermes inputs exercise replay dedup without an assistant clearing the look-back.
Cursor inline and legacy conversation shapes store the same growing history in one SQLite
cell; their bounded replay is included alongside external bubbles. OpenCode's many-parts shape
is deliberately outside the gate because one emitted logical message owns its demanded parts.

CI runs `python3 tools/perf-sqlite-check.py --probe MMAP_HEAP_PROBE`. The ten cases cover these
five formats at 8/32 MB, with approximately 128 KB payloads per row. The bound is 2 MiB plus
2 KiB per structural row: compact Devin forests and Cursor headers may grow, but retaining whole
payloads cannot fit. Feature/counter validation rejects an uninstrumented binary, callback
counts prevent skipped output, and owned temporary directories must be empty after each process.
The driver removes only its own sources by default; `--out NEW_DIR` retains evidence. Native
SQLite allocation and mmap residency are excluded. `--baseline OLD_PROBE` checks preserved-order
message/metadata digests in separate runs; `--escaped` supports a focused decoding check.

`perf-fixture.py claude|codex NEW_DIR --shape usage --records CALLS` generates tiny provider usage
records. Claude repeats each response ID in two assistant records; Codex emits independent
last-usage records with cumulative totals. The manifest gives independent expected fields and
duplicates; payload bytes are not the scale knob. `check-heap-budget.py ... --only stats` runs
16k/64k calls in native and portable processes with a 2 MiB ceiling and 256 KiB growth allowance,
checks exact provider totals and model/harness buckets, and verifies ordinary temporary cleanup.
This bound assumes tiny usage records and a fixed model roster, not arbitrary output cardinality.

`check-heap-budget.py ... --only selected-docs` gates Continue and LM Studio discovery and lazy
discard in native and portable builds. Four records contain 16/64 MiB of plain or escaped payload;
the bound is 2 MiB with 256 KiB growth. Expected callback and discovered-session counts prevent
empty reads from passing. These named-document fixtures isolate selected-body retention, not
arbitrary rich metadata, Harmony decoding or full output. The portable LM Studio whole-file read
previously failed at 16.8/67.1 MB; the source-view scanner stays below 329 KB in these cases.

The separate search CI oracle uses `--shape sweep-arguments --payload-bytes 100000000` and four
logical copies plus 32 tiny sessions. It executes production planning, parse, tee, queue, tokenizing,
commit and compaction with actual 4 MiB chunk settings, independent full-body hashes and freshness
checks. The existing 384 MiB additional live Rust heap ceiling includes Tantivy's Rust arenas;
the queue charge alone is capped at 64 MiB. A compact document can reach about 16 MiB. This is
not a 4 MiB document limit or a total RSS/native heap guarantee. See the search example's
`sweep_probe.md` for the exact isolated command and optional unbounded negative control.

The text span operations report demanded source failures. The prefix budget counts raw encoded
bytes, with UTF-8/escape boundary handling; it is distinct from `json-prefix`'s canonical output
budget. `prefix` retains the returned string through the heap snapshot and hashes it only with
`--hash-output`. It guards the whole supplied span even when the prefix budget is zero. None
proves historical source identity from an unstamped `Span`; the resolver captures identity at
first demand.

First-use metadata-cache allocation control uses `tools/profiling/discovery-cache.py` with two
`store_probe` binaries built with `--features probe-heap`. It creates a new synthetic home and
restores one canonical catalog path for every child, holding the one stale Claude inventory
constant while adding 4,096 unrelated serialized cache rows. The `sessions` operation follows
production fleet detection from `HOME`/XDG; its `--root` argument does not restrict that fleet.
The runner supplies a minimal isolated environment and discards elapsed-time fields. It reports
exact listing digests, count publication, allocation requests/bytes and peak requested Rust heap;
SQLite/C heap and retained serialized-row copying are separate limits.

`--sqlite-heap` adds SQLite's own allocator accounting to the same operation stage, independently
of `probe-heap`. It requires the `sqlite` feature and an isolated process whose SQLite has not yet
initialized. The option enables MEMSTATUS, initializes SQLite, and proves live-byte/live-allocation
counters work with a successful 4,096-byte allocation before setup. A failed/disabled counter is an
error, rather than a reported zero. After optional discovery setup, stage counters reset and report
baseline/end/peak live bytes, peak above baseline, live/peak allocation counts, and largest single
allocation request. Counts are simultaneous outstanding allocations; cumulative allocation requests
and requested bytes are unavailable from SQLite's built-in status API and not reported.

These counters include SQLite-managed page caches and SQLite's allocator bookkeeping, exclude
underlying system allocator overhead, and do not measure mapped pages or filesystem residency.
They are process-global, covering all SQLite connections used by the operation. They neither
replace the native allocator nor change connection read/provenance policy. See SQLite's
[memory accounting](https://www.sqlite.org/c3ref/memory_highwater.html) and
[status parameters](https://www.sqlite.org/c3ref/c_status_malloc_count.html).
`tools/perf-sqlite-native-heap.py` reuses existing OpenCode, Cursor IDE and Goose selected-payload
fixtures at two sizes with one giant record versus 64/256 smaller records. It checks exact
preserved-order message/metadata hashes against an explicitly supplied baseline in separate runs.
