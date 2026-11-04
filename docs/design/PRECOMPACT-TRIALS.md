# Precompaction trials

Throwaway sessions, never a seat. `PRECOMPACT.md` is the design.

## Second trial (2026-11-04, Claude Code 2.1.318)

Real windows resumed, one run each, scored by the author. Probes are seed-sampled from the prior
native summary and the window, answered with no tools. Window B's probes and key were written and
hashed before any contestant summary existed; window A's were sampled before, but the author read
that key before building this design, so A informs and B judges.

| Window B, 342k | stall | tokens after | this window (8) | earlier generation (6) | route tokens in / out | route time |
|---|---|---|---|---|---|---|
| native | 48.2 s | 13.8k | 2, 1 partial | 1 | | |
| cv, gpt-6.1-sol | 242 ms | 11.5k | 6 | 2 | 317k / 4.7k | 102 s |
| cv, devin/swe-2 | 176 ms | 16.9k | 5, 1 partial | 2 | 325k / 9.3k | 58 s |
| cv, devin/swe-2-flash | 76 ms | 11.2k | 6 | 1 | 158k / 3.8k, extraction pass failed | 435 s |
| cv, grok-build-0.1 | 72 ms | 15.3k | 5, 1 partial | 1 | 309k / 7.7k | 58 s |

| Window A, 343k | stall | tokens after | this window (6) | earlier generation (12) | route tokens in / out | route time |
|---|---|---|---|---|---|---|
| native | 73.0 s | 36.9k | 3, 1 partial | 3, 1 partial | | |
| cv, gpt-6.1-sol | 4.9 s | 20.7k | 6 | 9, 1 partial | 256k / 11.1k | 229 s |
| cv, devin/swe-2 | 564 ms | 20.5k | 6 | 7 | 261k / 17.7k | 107 s |
| cv, grok-build-0.1 | 438 ms | 15.4k | 6 | 1, 2 partial, 1 wrong | 250k / 6.1k | 46 s |

No probe on B drew a wrong answer: every contestant said "not in context" on what it lacked. The
stall is `durationMs`, on a loaded host, with a wrapper and a hook logger in it: `commit` itself took
4-98 ms, and a loaded run read 3.4-4.9 s. The canary reads this without those.

The veto, one compaction at 190-198k, the same prompts: flag off native 49.7 s; flag on native 11.6 s
and 9.9 s (`precomputed: true`, its summary paid for); flag on with `serve = cv` 3.4 s, the
precompute skipped by the harness's own log and `PreCompact` absent, with 139k in and 17k out on the
route across 9 passes. One check, one commit; no miss.

`PostCompact`'s `compact_summary` on 2.1.318: flag off native 14,691 chars; flag on native 13,050
and 22,481; manual native 1,501; served, empty, every time.

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
