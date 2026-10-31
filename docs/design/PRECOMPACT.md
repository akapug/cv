# Precompaction: cv's compaction, inside the harness's own

Status: built on akapug's fleet (2026-10-28, reshaped to the 0.19 design record 2026-10-29); first
served live on throwaway sessions 2026-10-31, on no seat yet.
Paths are relative to `crates/`. The 0.19 record is `docs/design/0.19/`; its REVIEW-ASTRA §3
handoff (prepare, ready, commit, miss) is the shape this page follows.

## The goal

A compaction cv controls end to end, measured against the harness's own. Claude Code's native
compaction (its background precompute included) is the baseline: it serves a seat for as long as it
measures better. A per-seat setting chooses which one serves, by `report`. cv starts from what it
alone has: the whole transcript, so a summary can carry pointers back into it (`[m:ID]`, opened with
`cv show <session> --around ID`) instead of retelling it.

## The stall

From the `compact_boundary` records' `compactMetadata.durationMs` in 31 transcripts on our fleet
(5,159 compactions, 2026-06-27 to 2026-10-28):

| Seats | Compactions | Median | p90 |
|---|---|---|---|
| Claude Code through our proxy to a Codex model ("codex seats"), auto | 2,996 | 335 s | 508 s |
| Claude Code on Anthropic models, auto, harness-written | 1,900 | 72 s | 99 s |
| Claude Code on Anthropic models, auto, `precomputed: true` | 212 | 0.12 s | 78 s |
| Claude Code, manual `/compact` | 51 | 108 s (Anthropic), 168 s (codex) | |

The third row is Claude Code's native precompute: the latency to beat is already near zero where
it is on, so cv has to win on what the compaction keeps.

## The handoff

| step | where | what it does |
|---|---|---|
| prepare | the follower, `cv watch --kind precompact` | at 70% of the learned auto-compact point, then every 20k tokens or 45 s idle: takes the session's lease (`store::take_lease`, one owner per context generation on the host) and cuts the window at its newest message |
| ready | the same | writes the summary (two model passes, below) and publishes the `Record` whole, keyed by `Key`: a cv checkpoint of the transcript (`FileId` plus the sampled `Prefix`) and the harness's own id of the cut message (Claude Code's record uuid). Never a hash of the IR |
| commit | `cv watch --kind precompact commit`, called by the harness plugin inside the harness's compaction | `precompact::commit`: the checkpoint still admits the transcript; the record is under 7 days old; the compaction has no `/compact` instructions of its own; every content commitment (prose segment, tool call id) is in the conversation being compacted, in order; the kept messages start on a whole tool call; summary plus kept messages fit `fit_fraction` (0.5) of the auto-compact point |
| miss | the plugin | any miss, error or the deadline (5 s): `next(e)`, the harness compacts as its own, native precompute included. Nothing waits on cv |

The checkpoint accelerates the check and never authorizes the swap alone: a rewrite outside the
sampled bytes passes it, so the commitments are compared against the conversation the harness
actually holds. Miss classes (what `report` counts): `no_record`, `no_key`, `checkpoint_refused`,
`too_old`, `focus_instructions`, `covered_messages_changed`, `would_not_fit`, `serve_native`.

No proxy sits in the path; the first build's `answer` action is removed and names `commit` (exit 2).

## Serving on Claude Code

`integrations/claude-code/cv-compact` is a Claude Code plugin (a hooks module, Claude Code
2.1.312's `session.compact` event). On each compaction of the main conversation (trigger `auto`,
`manual`, `plugin`) it reads its `serve` option:

- `native`: `next(e)`; cv only records the compaction.
- `cv`: runs `commit` with the conversation (`role`, `text`, tool call and result ids, sizes; not
  the bulk); on a pass returns the summary as one message followed by the kept messages, by their
  handles; on a miss, `next(e)`.
- `host` (default): `commit` without `--serve`, so cv's `[precompact] serve` decides.

`precompute` passes through untouched: the harness's own background summary stays the miss path's
fast answer (a second summary per window, and a served compaction waits for it: see the trial).
Subagent compactions pass through. A hook that throws is caught to `next(e)`. After every
compaction, cv's or native, the plugin runs `observe` without waiting on it.

The plugin's tests (`claude plugin test`) cover serve, miss and native.

## What the summary carries

Measured on our fleet over 7 days (2026-10-29): 34 times an agent lost something it had held; by
class, a settled rule broken again 15, built work claimed absent 6, our own terms lost 5, wrong
self or setup facts 4, in-flight work or a promise lost 4. Loss compounds over generations rather
than spiking at a seam. Public evidence agrees: a compactor keeps about 17% of side constraints, a
separate extractor appended after the summary over 90% (arXiv 2608.11242).

So a record is three parts, rendered in this order into the text served:

1. **Narrative** (`prompt::CV_INSTRUCTION`): what the work is for, decisions with reasons, where it
   stands, files, open questions, next step. Every line cites `[m:ID]`; an existence, absence or
   pass claim cites the message holding its check or says "not verified: probe before claiming
   this". Extended incrementally: an extension reads the previous narrative and the span since.
   Every cite must name a message of the window in full (`carry::cites`): a unique prefix of 8 or
   more characters is completed, any other cite is removed and counted in the record
   (`unresolved_cites`), so no cite sends the reader to a message that is not there.
2. **Carried sections** (`precompact::carry`), verbatim, never fed back to a summarizer: the
   person's rulings, open promises, terms, self and setup facts. A separate extraction pass
   (`prompt::EXTRACT_INSTRUCTION`) returns quotes with the message id each is copied from; a quote
   is kept only when that message holds it, whitespace aside (`carry::verify`: an extractor cannot
   plant a fact), and a self or setup fact never quotes the agent's own reply (where it was
   assumed, not shown: a seat once carried its own wrong "I run with a 1M context"). Items merge
   into the previous generation's (`Carry::merge`). The extraction pass is shown the carried items and marks the ones the span fulfilled (`done`), said or relied
   on again (`used`) or, for a ruling, replaced by a later ruling of the person's
   (`superseded`); a mark counts only when it names a carried item and cites a message that can
   make it (`carry::verify_marks`), and a carried quote the span repeats word for word counts as
   used without one. An item no span has said or used for `retire_after` windows retires; a
   ruling never retires by age, only by supersession. A quote shaped like an output line (a
   leading timestamp, a `unit[pid]:` tag, a one-line commit: `carry::noise`) is not carried. Live
   identifiers (commit shas, `task/N`, absolute paths, URLs) are read by shape with no model, each
   with where it was last seen, under "not verified now: probe before claiming one exists or is
   absent". A session moving to cv from the harness's own compaction brings a summary no carry
   holds, so the extraction also copies the lines of it that still bind (`standing`): only from a
   summary the harness wrote, within `standing_bytes`, marked as of that summary. A summary cv
   served comes back as a plain message of the person's; its opening (`SERVED_PREAMBLE`) keeps it
   out of the person's words and out of what counts as said again. The carry outlives the window
   (`carry/<harness>/<session>.json`): the next window's first summary starts from it. A record
   no follower saw compacted (a restarted follower, or `now` run once per window) is folded into
   that carry by the next read (`Live::adopt_stored`).
3. **Pointers to the standing rules the work touched**: ids only, never text (below).
4. **The person's words**, verbatim (`prompt::owner_words`): the window's prompts, harness markup
   cut, newest kept within 16 KiB; the rest named by id. They include what the person sent while
   the agent worked (Claude Code's `queued_command` attachment with origin `human`,
   `window::queued_prompt`), which the summary model also reads whole; a scheduled wakeup's
   prompt (cron, loop) is the harness's, not the person's.

**Which rules come back.** Rules arrive in three layers, and cv re-introduces only the middle one.
The base layer (instruction files, memory, skills) is re-injected by the harness after every
compaction: cv copies none of it. The per-turn layer (rules a hook injects when a turn calls for
them, stop guards) keeps firing after the seam: cv does not duplicate it. The contextual layer is
what a compaction loses, and it is chosen by what the span was doing, deterministically:

- the person's rulings in the span, verbatim: an extracted rule is kept only when it quotes a
  prompt the person wrote, outside the harness markup in it (`carry::verify` with `Layers`); no
  carried item may quote injected context. A ruling never filed anywhere exists only here;
- pointers to the standing rules the span's work acted on: the ids the configured
  `rule_patterns` find in what the agent did (its replies and tool calls, so a rule it fetched,
  quoted or named, and the refusals its calls drew), with hits and where last used. A rule a hook
  only injected is the per-turn layer and makes no pointer. The `carry_pointers` most recently
  used are kept. A fleet names its own patterns (a fetch verb among them); cv knows no rule
  store.

Each carried item and pointer records why it was chosen (said in this span at `[m:ID]`, carried
from an earlier summary, touched N times), in the record and in the `written` event, so an
evaluation can score the selection.

A captured harness instruction (`profile import`, then `precompact.profile = <id>`) replaces the
narrative instruction only.

**The seam for evaluation.** `cv watch --kind precompact now <id> --upto-bytes N --print` writes
the summary of the window as it stood at byte N to stdout, touching no store: the 0.19 record's M7
(probes from the dropped span, cv against the harness's default) runs through it.

## Recording and comparing

`events.jsonl` under the store holds: `written` (each summary), `commit` (served or missed, with
class, kept messages, estimated tokens, record age), `compaction` (the follower, from the
transcript's own boundary), `compacted` (the plugin: method, wall ms, tokens and messages before and
after, summary bytes, the miss that sent it native), and `swap` (lineage of an explicit new session).
`cv watch --kind precompact report` folds them into one row per source and method (`cv`, `native
precompute`, `native`): compactions, stall median and max, tokens before and after.

## One method per seat

`serve` picks who serves; `prepare_native` (default off) picks whether cv prepares where the
harness precomputes its own (read from the newest compaction's `compactMetadata.precomputed` or
the profile's `cachedGrowthBookFeatures.tengu_sepia_moth`, `Adapter::native_precompaction`). Off,
cv stays idle on such sessions, as "observe native first" asks; on, a seat set to `cv` gets cv's.

## The fallback: an explicit new session

`swap <id>` writes a new session that starts at a compaction boundary with the record's summary
and the records after the cut (Claude Code only), and records the lineage. It commits only the
generation it checked. On a real 4.8 MB session at 350k tokens: 0.27 s; `claude --resume` on it
answered in 1.85 s.

## Thresholds

| Setting | Default | Class | Effect |
|---|---|---|---|
| `serve` | `native` | policy | which compaction serves; a seat's plugin option wins |
| `prepare_native` | false | policy | prepare where the harness precomputes its own |
| `start_fraction` | 0.70 | budget | first summary at this fraction of the auto-compact point |
| `compact_at` | learned | — | the session's own `preTokens`, else the lowest seen for its harness and model |
| `extend_tokens` / `idle_secs` | 20,000 / 45 | budget | when to extend |
| `tool_result_bytes` | 2,048 | budget | each tool result's head and tail the summary model reads |
| `owner_words_bytes` | 16,384 | budget | the person's words carried verbatim |
| `carry_ids` | 25 | budget | live identifiers carried, most recently seen kept |
| `carry_pointers` | 20 | budget | standing-rule pointers carried, most recently used kept |
| `standing_bytes` | 12,288 | budget | lines carried from the harness's own summary, newest kept |
| `retire_after` | 3 | budget | windows an unused carried item stays (rulings leave only when superseded) |
| `rule_patterns` | none | policy | regexes finding a standing rule's id (first capture group); `--rule-pattern` |
| `fit_fraction` | 0.5 | safety cap | summary plus kept messages, of the auto-compact point |
| `jobs` | 2 | safety cap | summary jobs at once |
| `live_within_secs` | 7,200 | budget | sessions followed at startup |
| `MAX_AGE_SECS` | 7 days | safety cap | Claude Code's own `too_old` |
| `SUMMARY_TIMEOUT` | 900 s | safety cap | a model pass past it fails; the session keeps its old record |
| `LEASE_STALE` | 960 s | cost-only | a lease older than this is a dead owner's |
| `RETRY_FIRST` / `RETRY_MAX` | 5 s / 600 s | cost-only | a read refused for a changed source is retried, doubling |

## First live trial (2026-10-31, Claude Code 2.1.316, throwaway sessions, never a seat)

| | stall (`durationMs`) | tokens after |
|---|---|---|
| cv served, 118-133k window, 3 auto compactions | 35, 47, 63 ms | 26.6-29.8k |
| cv served, a real 354k window resumed | 46 ms | 13.0k |
| native on a miss, or set native, 114-131k window, 7 compactions | 31-75 s | 8.7-29.6k |
| native on a miss, the same 354k window | 83.8 s | 32.2k |

Misses seen live, each falling through to native: `no_record`, `would_not_fit`,
`covered_messages_changed` (a scheduled prompt, now not anchored), a hung cv (the 5 s deadline, added
to the stall), unparsable output, a crash.

What the trial found about serving: `PreCompact` runs beneath `next(e)`, so a served compaction
never fires it (`PostCompact` and `SessionStart` `compact` do, in that order, the reverse of
native's); with the flag on it fires instead when the precompute starts, a minute or more before.
The base layer's rows (`instructions`, `session_context`, `date`) return at the next prompt, not at
the boundary, so a compaction in the middle of a long turn runs without them until the turn ends.

Fidelity, probes answered with no tools. On the 354k window the first build held the span 8 of 9
(native 8) but what only the previous native summary held 0 of 5 (native 4): the narrative is told
not to list it, a ruling may not quote a summary, and a first generation has no carry. With
`standing`, 4 of 5 and the span 8 of 9, at 16.5k tokens after. Blind on a second window (343k; 18
items sampled by seed before either summary was read): earlier generation cv 6 of 12 with 2
partial, native 2 with 1; this window's items cv 2 of 6, native 4; tokens after 9.0k against 31.1k,
stall 165 ms against 61.0 s. So cv keeps more of the old at a third of the size and thins the
span's shas and counts. Planted items on a fresh session (first build): 9 of 10 against 10.

With `tengu_sepia_moth` on (one run each, one 190-198k compaction, 9 turns): off 44.9 s, 10.8k
after, $3.96; on 43 ms, 32.1k after, $3.85; on with `serve = cv` 10.6 s, 18.0k after, $4.11 plus
$1.51 of cv's summaries: `commit` took 31 ms, but the harness first waited out its in-flight
precompute, then threw it away. It never arms under a 200k window, which is why 150k arms show
nothing.

## Measurements kept from the first build

- **Commitment replay.** 26 real compactions from 4 codex seats and 3 Claude Code sessions,
  resumed against a mock to capture the request Claude Code 2.1.312 built: a cut at the newest
  message matched 24 of 26 (the two resumed a different leaf) with a median 2-message tail; a cut
  at 70% matched 26 of 26 (median 126 messages, 192 KB of tail). The redacted pairs are
  `cv-core/tests/fixtures/precompact/`, replayed by `tests/precompact_replay.rs`.
- **Watch cost.** On the hub, 11 live sessions for 10 minutes: 160.5 s of CPU before the append-only
  read, 4.9 s after; 0.6 s per 5 minutes at rest, 48 MiB.

## Against the 0.19 design record

| 0.19 record | the first build (fork lane, to 19e91340) | now |
|---|---|---|
| key: a cv checkpoint plus the harness's anchor id, never an IR hash | record keyed by session; validity by content anchors only | `Key { source, file, prefix, anchor }`; content anchors are the commit's commitments |
| no proxy in the critical path | `answer` for a proxy, with a model tail pass at answer time | `commit` inside `session.compact`; no model call in the critical path; `answer` removed |
| prepare, ready, commit, miss | write, serve on match | lease, atomic ready record, commit check, native on every miss within a deadline |
| observe native first | idle where native precomputes | the same by default; `serve` and `prepare_native` per seat, `report` compares |
| summary: fixed sections, identifiers verbatim, anchors | the harness's captured instruction | narrative with `[m:ID]` citations, verbatim carried sections, the person's words |
| sidecar `version`, discard on mismatch | `RECORD_VERSION` 1 | 2 |
| D4: a `cv watch` consumer | followed | followed |

## What 0.19's watching would need

A wake for resident readers (`watch::wake`); a per-message hook with an "accepted through N"
callback and positions kept for paused sessions (`Live::take`, `settle`, `restart`); the token fill
and the compaction point as first-class facts; harness facts behind the adapter; a byte-prefix read
for replay (`Start::Prefix`); a read of an append-only transcript that grew during the read (built
here: 26.7% of a core to 0.81% on the hub); a streaming poll (a batched one is quadratic).
