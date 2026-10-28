# Precompaction: a compaction summary written before the harness asks

Status: built on a fork lane and measured on our own fleet (2026-10-28), offered as input for
v19's watching and live ingestion. This page says what we built, why it has the shape it has,
and what real use measured. Paths are relative to `crates/`.

## The stall

When a harness's context window fills, it stops the session, sends the whole window to a model
with its own summary instruction, and waits. Measured from the `compact_boundary` records'
`compactMetadata.durationMs` in 31 transcripts on our fleet (5,159 compactions, 2026-06-27 to
2026-10-28):

| Seats | Compactions | Median | p90 |
|---|---|---|---|
| Claude Code through our proxy to a Codex model ("codex seats"), auto | 2,996 | 335 s | 508 s |
| Claude Code on Anthropic models, auto, harness-written | 1,900 | 72 s | 99 s |
| Claude Code on Anthropic models, auto, `precomputed: true` | 212 | 0.12 s | 78 s |
| Claude Code, manual `/compact` | 51 | 108 s (Anthropic), 168 s (codex) | |

The third row is Claude Code's own precompute (feature `tengu_sepia_moth`): the summary is
written in the background before the window fills, and the stall is gone when it is current. It
is the reference this work measures itself against, and on a seat where it is on, cv stays out of
the way (see "One method per seat").

## What we built

`cv precompact` keeps a summary of every live session's current window, written early and kept
current, and answers the harness's compaction request from it when the request proves to be the
same conversation. Three parts:

1. **`cv_core::precompact`** (`cv-core/src/precompact*`): pure and unit-tested. A record
   (`Record`: session, window, cut, anchors, instruction profile, model, text), a window tracker
   (`Window`) fed the IR message stream, the anchor fingerprint and `validate(request, record)`,
   the summary prompts, and a file store under `$CLUSTERVISION_HOME/precompact/`. No harness
   name appears in it; the two harness facts it needs are `Adapter` methods.
2. **`cv precompact watch`** (`cv/src/cmd/precompact.rs`): one resident process on the existing
   `Watcher`. It reads each live session's appended records, tracks its window and token fill,
   writes the first summary at 70% of the auto-compact point, and extends it as the session
   grows or goes idle. Summaries are written by `cv-llm` (the `LMSTUDIO_API_BASE` provider, so a
   local model or our proxy's OpenAI-compatible endpoint; the model is a setting, and cv-llm's
   egress notice is printed).
3. **`cv precompact answer`**: the request on stdin, the summary on stdout, exit 3 with the reason
   on stderr on a miss. A proxy (or any other caller) runs it on a detected compaction request
   and falls through to its normal route on a miss, an error or a timeout. Our CLIProxyAPI fork
   does this in one handler (`claude-code.compaction.precompact.command`).

Also: `now` (write one session's summary now), `check` (one request against one session, a JSON
verdict, no model call), `profile import|ls`, `ls`, `report` (stall medians by method, served and
missed counts, miss reasons) and `swap` (the fallback below).

### Never a stale summary

A summary is served only when the request the harness actually sent contains, in order, every
anchor the summary was cut at. Anchors are the parts of a conversation a harness does not
rewrite when it builds a request:

- **Prose**: what the person typed and what the model said, cut into segments at harness markup
  (`<name …>…</name>` with a lowercase name, cut by shape, not by a tag list), whitespace
  normalized, segments under 12 bytes dropped. Tool output, images, thinking, injected context
  (system reminders, task notifications) and harness notices are not anchors: a harness
  truncates, strips, injects and merges those.
- **Tool call ids**: the id the provider gave each call, which every later request echoes
  (`tool_use.id`, Responses `call_id`, Chat Completions `tool_calls[].id`). A window of tool work
  with little prose would otherwise pin its cut only at the last sentence the model wrote; the
  first replay below had a codex seat whose last anchor sat 116 messages before its cut.

An anchor is kept as a length, a rolling hash and a SHA-256 prefix, never as text. Matching is
Rabin-Karp over the request's normalized prose with the SHA confirming a hit, O(request prose +
anchor bytes). The summary's instruction is part of the match too: a summary written to one
instruction (a captured "profile") is not served for a request carrying another.

On a match, the messages after the last anchor are the tail. An empty tail serves the summary as
written. A non-empty tail gets one fast-model pass over the tail only, spliced into the summary
under "Since the summary above". A miss names the first anchor that was not found.

### What the watch costs

Measured from `/proc` on our hub (a host at load 50-70, the watch at nice 19) with `--dry-run`,
which reads and tracks exactly as a real watch and makes no model calls:

| | wall | CPU | RSS |
|---|---|---|---|
| startup and priming: discovery in a fresh `CLUSTERVISION_HOME`, then 9 live sessions read whole (the largest 2.28 GB) | 71 s | 153 s | 84 MiB |
| at rest, nothing followed (`--cwd` matching nothing), the rest of the fleet writing | 300 s | 0.41 s (0.14%) | 28 MiB |
| following 9 live sessions, between retries of the largest | 120 s | 0.04 s | 90-95 MiB |
| following 9 live sessions, 10 minutes | 600 s | 155 s (26%) | 84-107 MiB |

The watch sleeps in `inotify` on 544 directories (each store root down to where sessions live,
so a session in a new project is followed within a second). A write to a file it does not follow
is looked up once and then costs a wake and a set lookup; the 0.41 s at rest is those lookups
for the sessions other seats started in those 5 minutes (an earlier build that watched only the
store root measured 0.00 s in the same 5 minutes).

Everything above the plateau is the 2.28 GB transcript of the seat that ran the measurement,
written to several times a minute: its read never completes (item 6 under "What v19's watching
would need"), so each retry rereads it whole, 15-35 s of CPU. The retry doubles from 5 s to 10
minutes, so that one session settles at one whole read per 10 minutes; every other session costs
a resumed read of its appended lines.

Two earlier shapes of the loop, for the record: polling in batches of 2,000 messages never
finished priming the same fleet in 30 minutes (42% of a core throughout, item 8), and letting a
failed session go for good dropped the very sessions that compact most.

## Measurements

### Probes

- **Session identification.** Claude Code names its session in the compaction request three
  ways: the `X-Claude-Code-Session-Id` header, `metadata.user_id` (a JSON string with
  `session_id`), and the transcript file name. Codex seats are Claude Code behind our proxy, so
  their request is the same shape. The request carries no message ids, which is why matching is
  by content.
- **The response shape the harness accepts.** A Messages response or event stream with one text
  block holding the summary (usage zero) is recorded by Claude Code 2.1.312 as an ordinary
  compaction: a `compact_boundary` record and an `isCompactSummary` user record with the text.
- **Claude Code's PreCompact hook cannot supply the summary.** In the 2.1.311 bundle the hook's
  result carries new custom instructions, a display message and a block decision; nothing is read
  as summary text. This matches `HARNESS-ASKS.md` K1. The answer has to come from the request
  path (a proxy), or from the harness's own precompute.
- **Codex CLI's own compaction request** was not probed: no Codex CLI seat has run on this fleet
  since 2026-10-21. The request reader takes the Responses `input[]` shape by structure, and is
  unit-tested on it, not on a captured request.

### Anchor replay

28 real compactions picked from 4 codex seats and 3 sessions in `~/.claude`,
each replayed by copying the transcript as it stood at the compaction into a scratch Claude
config, resuming it with `claude --resume <id> -p /compact` against a local mock, and capturing
the request Claude Code 2.1.312 built. Two failed to resume at offset 0, leaving 26 (23 distinct;
three recur in a forked copy of the same session). `cv precompact check` then cut a summary at the
window's newest message ("end", a summary written just before compaction) and at the last message
under 70% of the compaction's `preTokens` ("70%", the earliest summary the watch writes):

| | match | tail after the last anchor, median | largest tail |
|---|---|---|---|
| end, prose anchors only, before the error fix | 22 / 26 | 36 messages, 78 KB | 674 KB |
| end, prose + tool call ids | 24 / 26 | 2 messages, 2.9 KB | 40 KB |
| 70%, prose + tool call ids | 26 / 26 | 126 messages, 192 KB | 368 KB |

The first row is the first build. The error fix below turned cases 22 and 26 from misses into
matches with a 2-message tail; the two misses left are cases 17 and 19.

The misses:

- **A prompt that drew an API error** (cases 22 and 26, one session): a scheduled prompt hit a
  usage limit, Claude Code parented the next turn above it, and the same prompt text came back
  later, so its anchor matched there and every anchor after it was out of order. Fixed: an error
  drops the anchors of the prompts no reply has answered yet (queued prompts are one turn).
- **The replay resumed a different leaf** (cases 17 and 19, end cut only): the request ended at a
  tool result before the transcript's newest assistant text, which was absent from the request.
  The miss is right; it is an artifact of resuming a copy, not of a live compaction.

The tool call ids are what made the tail small: on codex seats the model writes little prose
between tool calls, and with prose alone one seat's last anchor sat 116 messages and 674 KB
before its cut. The 70% row is the worst case: the watch extends the summary every 20,000 tokens
or 45 s of idle after that, so the tail at compaction is what accrued since the last extension.

### End to end

No live seat was touched. A codex seat's real session (case 0, 4.8 MB, a window of 351k tokens)
was copied into a scratch Claude config; `cv precompact now` wrote its summary (the summary model
a local mock); the lane's proxy build, with `claude-code.compaction.precompact.command` set,
answered Claude Code 2.1.312's `/compact` through `cv precompact answer`; Claude Code recorded the
compaction. The stall is the boundary record's own `durationMs`:

| scenario | answer | `durationMs` | the seat's median, harness-written |
|---|---|---|---|
| summary at the newest message (2-message tail) | served, tail pass by the mock, 1.1 s | 2,709 | 335,011 |
| summary 60 records early (8-message tail) | served, tail pass by the mock, 0.04 s | 3,155 | |
| the same, tail pass by `gpt-6-luna` through a codex seat's proxy | served, 6.2 s | 7,286 | |
| the same, tail pass by a local 1.9 GB qwen2 on CPU | served, 78.6 s | 79,948 | |
| the same, the tail pass's endpoint refusing (no key, then nothing listening) | miss, fell through in 0.2-0.6 s | (the harness's own) | |

About 2.7 s of `durationMs` is Claude Code's own work around the call (the answer itself left the
proxy in 0.4-1.2 s). The `gpt-6-luna` row is the real-model number: an 8-message tail updated in
6.2 s, against the seat's 335 s median. The qwen2 row is the warning: the tail pass is as slow as
its model, so it wants the fastest model the proxy can route, and the watch's extensions keep the
tail short. The last row is the fail-open path: Claude Code retried its compaction three times,
each answer missed in under a second, and each request went on to the proxy's own route.
In every row the stored summary itself came from the mock. Written for real, it is the watch's
background cost, not the stall: `cv precompact now` on the same window (350k tokens, rendered to
731 KB with tool results cut to 2 KB each) had `gpt-6-luna` write an 18 KB summary in 89.5 s,
where the harness's own compaction on that seat takes a median of 335 s with the stall on.

The fallback, on the same session: `cv precompact swap` wrote a 34-record, 18.5 KB compacted copy
of the 4.8 MB transcript in 0.27 s (one streaming pass; only the records after the cut are held),
and `claude --resume <copy>` started from it in 1.9 s (its first request carried the summary and 3
messages).

## One method per seat

Claude Code's native precompute and cv's must never both run for one session. The rule, as
built:

1. **Native first.** cv reads, per session, whether the harness precomputes: the newest
   compaction's `compactMetadata.precomputed`, or the feature flag in the Claude Code profile's
   `.claude.json` (`cachedGrowthBookFeatures.tengu_sepia_moth`, read once per file state). Either
   one, and cv writes nothing for that session (`Due::Wait("the harness precomputes its own
   compactions")`). The flag is `Adapter::native_precompaction`, so a harness that gains a native
   path later says so in its adapter.
2. **Native is the reference.** The same `durationMs` metric is reported for both
   (`cv precompact report` splits stalls into "cv precompact", "native precompute" and "the
   harness's own"), and the fallback below copies the native result's shape (a boundary record,
   then the summary as a user record).
3. **cv's targets** are harnesses with no native path (Codex CLI, Devin, the other harnesses cv
   reads) and Claude Code seats where the flag is off.

## The fallback: a compacted copy

Where no proxy sits in the request path, `cv precompact swap <id>` writes a new session that
starts at a compaction boundary with the precomputed summary and carries the records after the
cut, and prints the command to resume it (`Adapter::compacted_copy`; Claude Code only today). It
replaces the stall with a resume, at the cost of a new session id. Measured on a real 4.8 MB
session at 350k tokens: the copy took 0.27 s and held 34 records (18.5 KB: the boundary, the
summary and the 32 records after the cut), and `claude --resume` on it answered in 1.85 s with a
3-message request that opened with the summary.

## Primitives used, and what we had to build

Used as they are on `upstream/main`:

- `Watcher::with_discovery` with an emit-existing discovery closure, and
  `try_poll_messages`: incremental per-session cursors, one message at a time, appended records
  only.
- `cv_core::sessions()` for discovery and rediscovery; `Adapter::try_stream` and
  `Adapter::transcript(…, Start::Prefix(len))` (a session as it stood at a byte length, for
  replay); `Demand::RICH`; `Resolver` for lazily held text.
- `publish::publish_reader` / `Claim::New` and `port::resume_lines` for the compacted copy;
  `restamp_line` for its records; `env::frozen` for `--frozen`.
- `cv-llm`'s provider selection and egress notice (`llm.rs`).

Built, because nothing upstream did it:

- **A wake** (`watch/wake.rs`): inotify on each followed session's directory plus an eventfd a
  job thread can poke, with a timer fallback off Linux. `Watcher` polls; a resident process that
  must cost nothing at rest needs to sleep until a file changes. This is the piece v19's watching
  most directly needs (below).
- **A window tracker over the IR** (`precompact/window.rs`): compaction boundaries, the token fill
  from the newest usage record (never an estimate), the learned auto-compact point
  (`preTokens`), and anchors, all updated per message.
- **A request reader** (`precompact/request.rs`) for three wire shapes by structure.
- **`Adapter::native_precompaction`** and **`Adapter::compacted_copy`**, so the CLI names no
  harness.
- `llm::complete`: a chat call with a caller-set timeout and token cap.

## Thresholds

Each is a named setting (`precompact` in the config file, or a flag), in COMPLEXITY's classes:

| Setting | Default | Class | Effect |
|---|---|---|---|
| `start_fraction` | 0.70 | budget | first summary at 70% of the auto-compact point; `ls` shows each session's fill and point |
| `compact_at` | learned | — | the auto-compact point: the session's own `preTokens`, else the lowest seen for its harness and model; unknown means wait, said in `ls` |
| `extend_tokens` | 20,000 | budget | extend when the fill grew this much since the cut |
| `idle_secs` | 45 | budget | or when the session has been idle this long with anything new |
| `tool_result_bytes` | 2,048 | budget | each tool result's head and tail sent to the summary model, the cut named in the prompt |
| `jobs` | 2 | safety cap | summary calls at once; a due session waits its turn and says so |
| `live_within_secs` | 7,200 | budget | sessions touched this recently at startup are followed; every new session after |
| `TAIL_TIMEOUT` / `SUMMARY_TIMEOUT` | 120 s / 900 s | safety cap | a model call past it fails; a failed answer is a miss, a failed summary is retried at the next due point |
| `RETRY_FIRST` / `RETRY_MAX` | 5 s / 600 s | cost-only | a session whose read failed (it changed while being read) is read again after 5 s, doubling to 600 s |

## What v19's watching would need

These are what this work needed and built for itself; they are input, not demands.

1. **Sleep until something changes.** A resident reader has to cost nothing at rest. `Watcher`
   polls; `wake.rs` is a working inotify + eventfd wake with a fallback timer, and it is
   cheap to fold into `Watcher` itself so every follower gets it.
2. **A per-message hook for incremental state.** The window tracker is a fold over messages as
   they arrive. A watching API that hands a consumer each appended message, with the session's
   cursor, makes that a few lines; today it is a `try_poll_messages` closure plus our own `Live`
   map.
3. **The token fill and the compaction point as first-class facts.** Every consumer that cares
   about context pressure re-derives them from usage records and `compactMetadata`.
4. **Harness facts behind the adapter.** Whether the harness precomputes its own compactions, and
   how to write a compacted copy, are adapter questions; we added two methods with `None`
   defaults.
5. **A byte-prefix read for replay** existed (`Start::Prefix`) and made the replay corpus cheap;
   it is worth keeping for every live feature's tests.
6. **A read of an append-only transcript that grew while it was read.** `watch::read::pass`
   refuses its whole pass when the primary file is not byte-identical at the end ("session
   source changed during read"), although the reader stopped at the length it captured and the
   adapter's prefix is immutable. A pass that takes longer than the gap between two appends
   never completes: on our hub the active seat's own transcript is 2.28 GB, and a batched
   `try_poll` re-read it from the start on every wake (18 GB read in 10 minutes, 42% of a core,
   never primed in 30 minutes). Accepting growth past the captured length for an append-only
   adapter, checked by the same `MarkHead` sample a resumed read uses, would let the first read
   complete. Until then `watch` waits a doubling retry on such a session (`RETRY_FIRST`,
   `RETRY_MAX`).
7. **Delivered is not accepted, and a position is forgotten.** `try_poll_messages` hands a
   consumer each message as it is read, but a pass that fails at its end (6) is delivered again
   from the last accepted position; and a session that leaves the discovery list (a retry wait,
   or idle past the live window) loses its position, so it is read from the start when it comes
   back. A fold over messages has to count what it took and skip the repeat (`Live::take`,
   `settle`, `restart`); before it did, one returning session logged 75 old compactions as new.
   A per-session "accepted through N" callback, and positions kept for sessions a follower
   pauses, would make that the watcher's job.
8. **A batched poll is quadratic on a long session.** `try_poll(Poll { max })` streams each
   session to EOF on every call and keeps `max` messages, so a session of N messages costs N/max
   whole reads; `watch` primed nothing on the live fleet that way. The streaming
   `try_poll_messages` reads it once.

## Against the 0.19 design record

`docs/design/0.19/` (b4674049, the same day) plans precompact as 0.19's first consumer of
checkpoints. This lane was built before it landed; where they meet:

| 0.19 record | this lane |
|---|---|
| §4c trigger: fill fraction and idle | the same: 70% of the learned auto-compact point, then every 20k tokens or 45 s idle |
| §4c handoff order: in-process first, proxy second (fails open), swap last | proxy and swap built; proxy fails open on a miss, an error or its timeout. The in-process `session.compact` plugin was not exercised |
| REVIEW-ASTRA §3: observe native Claude first | cv stays idle where Claude's native precompute is on, and `report` measures both by the same `durationMs` |
| §4b key: checkpoint plus the harness's anchor uuid | the record keeps the cut message's native id (`Cut::id`), but the proxy path matches by content: a compaction request carries no record ids, only the conversation |
| §4d sidecar `version`, discard on mismatch; transcript never edited; the summarizer's answer recorded | `RECORD_VERSION`, a record of another version is a miss; cv never writes the harness's transcript; the record holds the summary text and model |
| §4d incremental extension anchored to the parent | `Extend` summarizes from the cut on with the prior summary first |
| REVIEW-ASTRA §1: a stale accepted swap rejects the key | here a summary is served only when every covered prose segment and tool call id is in the request in order; a tool output the harness shortened since is not checked (the summary is not stale because the harness cut a log) |
| D4: no `cv precompact` verb; a `cv watch` consumer plus `--kind precompact` on `show` and the API | not followed yet: this lane has a top-level `cv precompact`. Folding `watch` into `cv watch` and `answer` behind the hook/proxy surface is a naming call for the 0.19 plan |

What this lane adds to §8's measurement plan: M1 for codex seats through the proxy (a baseline of
2,996 harness-written compactions at a 335 s median, against 2.7-3.2 s on the served path inside
Claude Code), M6's first reason
distribution (the replay's misses), and a replay corpus to run M7 against.

## The corpus

`cv-core/tests/fixtures/precompact/` holds real compactions redacted by
`tools/precompact-fixture.py`: every run of letters and digits replaced by a keyed pseudo-word of
the same shape, the same on both sides, ids shortened consistently, text blocks cut to 160
characters and tool results to 48 on both sides. `tests/precompact_replay.rs` replays each: the
newest cut matches with its tail, an earlier cut matches with a longer one, and an edited history
misses.
