# cv 0.19: from reader to recorder (design handoff, 2026-10-28)

For pug's team, who build 0.19 on pug's fleet. It stands alone; every claim names its source.
Source keys used throughout:

| key | what it is |
|---|---|
| **INJ** | `INJECTION-SURFACES.md` (this session's scratchpad, 2026-10-24): Claude Code 2.1.305-310 and Hermes 0.21.6 surfaces × lifecycle; `H/` = the Hermes 0.21.6 site-packages |
| **CC312** | the Claude Code 2.1.312 binary (`~/.local/share/claude/versions/2.1.312`), byte offsets |
| **cv** | `~/dev/cv` at `915ace06`; paths relative to `crates/` unless they start with `docs/` |
| **fn2/** | `git show origin/fn2/main:fn2/docs/design/<file>` in `~/dev/fn` |
| **pb/** | `~/paperbin/<file>` (local copies of papers and docs) |
| **HN n** | Hacker News item n (`news.ycombinator.com/item?id=n`), read through Scry |
| **OA W…** | OpenAlex work id (Scry `openalex.works`) |

What 0.18 already is: one IR, 21 adapters, one record codec per wire format, checkpoints
`(offset, per-layer state)` (`cv-core/src/stream.rs:425-433`) validated against one source identity
`FileId { len, mtime_ns, dev, ino, ctime_ns }` with growth admitted by content (docs/design/DATAPATHS.md
"Correctness boundaries" 2), and two append-only ledgers already on disk: `lanes.jsonl`
(`cv-core/src/lanes.rs:148-160`) and the task log (docs/INTERFACE-V2.md "The shape"). ember's own
post calls 0.18 "about the limit of the datamodel" without becoming more invasive (memory
`cv-0-19-direction.md`).

---

## 1. The role change

**Verdict: cv records what happens from the harnesses' contracts (hooks, OTel, plugins), keyed by
ids it assigns; harness stores become evidence it links to; launching is optional and adds only
what nothing else can.**

| role | what it sees | what it cannot see | cost to the user | source |
|---|---|---|---|---|
| reader (0.18) | everything the store keeps, after the fact | birth before the first record, intent, lane, cross-host parent, host, crash, Hermes per-call timing, Hermes hook-injected context | none | INJ §2, §4, §5 |
| recorder | + birth (Claude SessionStart; Hermes on first turn), every tool/model call as it happens, sub-agent edges, compaction, end reasons | crash, intent, cross-host parent, queue/node | Claude: ~33 hook entries + OTel env through managed settings; Hermes: `plugins.enabled` + a plugin dir | INJ §7a |
| launcher (optional) | + identity chosen before the first byte, intent/lane/parent/host stamped into env and `OTEL_RESOURCE_ATTRIBUTES`, exit status and signal, a per-request tag the cluster can log | queue position unless the cluster logs the tag | a wrapper on PATH, or `claude --session-id`, `hermes -c <id> --create-if-missing` | INJ §7b |

Coverage (INJ §7a): Claude ≈ 90% of lifecycle moments with all hooks + OTel; Hermes ≈ 60% reader-only,
≈ 85% with an in-process plugin. The remainder (crash, intent, cross-host parent) is causal, not
coverage: only process supervision and birth-time stamping close it.

## 2. Identity at birth

**Verdict: every event carries `(host, seq)` as its unique key and an HLC timestamp; a session is
`(host, harness, id)`; parentage is an explicit edge, never inferred from time.**

| field | meaning | assigned by | when | carried how |
|---|---|---|---|---|
| `host` | stable host id: a configured name, defaulting to a hash of `/etc/machine-id` (Linux) or `IOPlatformUUID` (macOS) | cv, once per host | install | every ledger row; hub path prefix |
| `incarnation` | ledger generation on that host (bumped if the ledger is lost or restored) | cv | ledger creation | every row |
| `seq` | per-(host, incarnation) dense counter | the local appender | each append | dedup key at the hub (Kafka's producer id + sequence: pb/kafka2026-docs-design.md:193) |
| `hlc` | hybrid logical clock (Kulkarni et al., "Logical Physical Clocks", OPODIS 2014, OA W2274037101) | the local appender, merging any HLC received in env/headers | each append | every row; `CV_HLC` env on spawn |
| `session` | harness-native id; launcher-chosen when launched (Claude `--session-id <uuid>`, Hermes `-c <id> --create-if-missing`) | harness, or launcher | birth | hook payload `session_id` (INJ §1 common fields), Hermes payloads |
| `parent` | `(host, harness, session, agent_id?)` of the spawner | launcher env `CV_PARENT`; in-host sub-agents from SubagentStart / subagent_start | spawn | row field; OTel resource attr |
| `intent` | one line: what this session is for | launcher flag or the spawner's task text | birth | row field |
| `lane`, `cluster` | the swarm lane and the fleet/cluster name | launcher / config | birth | row fields, `OTEL_RESOURCE_ATTRIBUTES` |
| `request_tag` | per-session tag the inference cluster logs | launcher (Claude `ANTHROPIC_CUSTOM_HEADERS`; Hermes LLM-request middleware `H/hermes_cli/middleware.py:24`) | birth | outbound HTTP header |

Why HLC and not wall time or Lamport alone:

| choice | what bit others | source |
|---|---|---|
| wall clock + last-writer-wins | Cassandra's LWW dropped 30-70% of acknowledged writes under partition and skew | Jepsen: Cassandra, aphyr.com/posts/294-jepsen-cassandra |
| span timestamps across hosts | Dapper notes cross-host timestamps are subject to skew and orders spans by the RPC tree, not by time | Sigelman et al., "Dapper", Google TR 2010 (OA W2171867449) |
| Lamport clock alone | orders causally (Lamport 1978, OA W3137220996) but loses wall-time meaning a human needs to read a fleet timeline | — |
| HLC | causal like Lamport, within NTP error of physical time, fits 64 bits | Kulkarni 2014 |

Rule: causal edges (`parent`, the hook's `agent_id`) order first; HLC breaks ties and orders
unrelated hosts; wall time is display only. Nothing merges by "newest timestamp wins" (§3).

Hermes ids are `YYYYMMDD_HHMMSS_<6 hex>` (24 random bits per second, `H/hermes_state_ids.py:56-60`):
unique enough per host (INJ §4 d: 7e-5 collision chance for 50 in one second) but not across 12 hosts
over months, so the global key is always `(host, harness, id)`.

## 3. The per-host ledger and the hub

**Verdict: one append-only ledger per host, mirrored to a hub by suffix; the catalog is a fold over
the ledger plus the stores; cv defines the append/read/wait/ack port and owns a file provider; fn
becomes a provider when its remote wait/ack serves.**

### 3a. What the ledger is in fn terms

fn's consumer model is `register / poll / wait / ack / position` with at-least-once delivery until
ack and exactly-once left to the consumer (fn2/CLIENTS-AND-DATA-MODELS.md:77-103). akapug's helm
reached the same port independently: `append(row) -> Evidence`, `read(room, after=row_id)`,
`wait(seat, timeout)`, `ack(seat, row_id)`, "so that fn, Mini or tmpfs become a provider choice"
(same file :216-219). fn's own verdict for that fleet: archive only today; remote wait/ack
"Realistically weeks, not days", sequenced after the storage landings (:254-258).

| concern | owner | why |
|---|---|---|
| append/read/wait/ack port | cv defines it (one trait), matching helm's port verbatim | two clients converged on it; fn's verbs map 1:1 (append = POST, read = poll, wait, ack) |
| provider: local file (JSONL, fsync per batch) | cv, 0.19 | `lanes.jsonl` and the task log are already this shape |
| provider: fn stream per host | fn, later | fn2's journal + cursors + acks is the durable form (fn2/STORAGE-DESIGN.md §1); not installable today (CLIENTS :236-241, :246-252) |
| harness readers, IR, codec, catalog, search, precompact | cv only | fn stores bytes and orders events; it does not interpret transcripts |

### 3b. Ledger row

`{host, incarnation, seq, hlc, wall, kind, session, harness, agent?, parent?, payload, payload_v,
source}` where `kind` is one of: `birth`, `prompt`, `model_call`, `tool`, `subagent_start`,
`subagent_stop`, `compact`, `precompact_ready`, `end`, `exit` (launcher), `store_seen` (reader),
`gap` (doctor). `payload` is the harness's own hook/OTel/plugin payload, raw, with `payload_v` =
`(harness, harness_version, surface)`; normalization happens at read (§6). `source` is
`hook|otel|plugin|launcher|reader`.

### 3c. Merge at the hub

Rows are immutable facts with a unique `(host, incarnation, seq)`: the hub's set is a grow-only set
(G-Set, Shapiro et al. 2011, pb/shapiro2011-comprehensive-study-crdts.txt:206), so any arrival order
converges and redelivery is idempotent. No LWW registers (Shapiro :915; Jepsen above). Mutable views
(lane status, "session ended") are folds over the set, as fn2 decision 1 states for keyed state
(`State = Fold(Summary_k, Events_{k+1..n})`, fn2/STORAGE-DESIGN.md §4.1). Local-first's "you own your
data" (Kleppmann et al., Onward! 2019, OA W2979846168) holds by construction: each host's ledger is
complete without the hub.

### 3d. Aggregation shape

| shape | precedent | lesson | for cv |
|---|---|---|---|
| **mirror by suffix** (ship the raw log tail, keep a cursor) | Dapper writes spans to local files and collects them out of band (Dapper §3); Kafka replication of a partition log; cv's own archive "verified suffix read" (docs/design/DATAPATHS.md, Archive row) | the producer never blocks on the collector; replay from cursor | **ledger: yes, always.** 3-7k rows/day per 30 seats (CLIENTS :222-224) is nothing |
| ship derived records | Canopy derives features near ingest (Kaldor et al., SOSP 2017, doi 10.1145/3132747.3132749); Scuba keeps sampled wide rows | derivations frozen at ingest cannot answer new questions; Canopy kept one underlying trace model for that reason | no: cv's IR changes per release; derive at the hub from mirrored rows |
| federate queries | Monarch: regional zones hold data, a global plane fans queries out (Adams et al., VLDB 2020, vldb.org/pvldb/vol13/p3181-adams.pdf) | worth it when data is too big to move; costs availability when a zone is down | only for cold transcript bytes not yet mirrored; never for the ledger (pug's boxes sleep) |

Transcripts (evidence) mirror lazily by suffix through the existing archive path, on demand or
idle. Identity must not be the path: session-roam synced `~/.claude/projects` across machines and
"requires same username and project paths" (HN 47728235); cv keys by `(host, harness, id)` and keeps
`path` host-relative.

Hub = `cv serve` with the hub routes (append-batch, read-after, wait) behind the existing API; `wait`
is a long-poll so no seat polls. helm measured the cost of polling: a 2.0 s tick, waiters at
"7-10% of a core per waiter", and set "wake p95 > 2 s" as a falsifier (CLIENTS :225-232).

### 3e. The 50-Hermes case, minimum path

From INJ §7d, in order; each step serves alone.
1. cv Hermes plugin: append `on_session_start`, `post_api_request` (endpoint, `api_duration`,
   `first_chunk_at`, usage; `H/agent/turn_response_intake.py:58-97`), `subagent_start/stop`,
   `pre_llm_call` (context Hermes does not store), `on_session_finalize` (not `on_session_end`,
   which fires once per turn, `H/agent/turn_finalizer.py:791-802`) to the host ledger.
2. SQLite reader: retry a failed `check_reads`; gate certificates on the per-session rowid range,
   not db+WAL file identity, so one writer of 50 stops invalidating 49 (INJ §7d;
   `cv-core/src/harness/store/sqlite.rs:181-186`, `harness/hermes.rs:662-684`).
3. Launcher shim: `hermes -c <cv-id> --create-if-missing --source cv` plus the request tag.
4. `host` in the IR (§5) so 12 ledgers merge.

## 4. Precompact: the first consumer of checkpoints as a public contract

**Verdict: build it; key the sidecar by a cv checkpoint plus the harness's own anchor uuid; hand
off in-process (Claude `session.compact` plugin event, Hermes `ContextEngine`) first, proxy second,
session swap last; validate with Claude's own rejection list.**

### 4a. What Claude Code already does (ground truth, CC312)

| element | Claude Code 2.1.312 | offset |
|---|---|---|
| sidecar file | `<project dir>/<sid>.precompact.json` (`BXe="precompact.json"`) | 200M+ region, `Qrn` |
| sidecar schema | `version` (=5), `sessionId`, `agentKey:"main"`, `model`, `cliVersion`, `createdAt`, `precomputedAtUuid`, `preCompactTokens`, `readyDurationMs`, `summaryText`, `summaryMessages[]`, `preserveUuids[]`, `attempt`, `groupsPreserved`, `totalGroups`, `totalUsage` | 229289270 (`Nlr`), validator 229283051 (`Tlr`) |
| arm threshold | `min(window − round(window × precomputeBufferFraction), window − 13000)`; also an idle trigger (`armTrigger` "idle", `tengu_idle_compact_fired`) | 228384050, 229290686 |
| key | the uuid of the last message summarized (`precomputedAtUuid`) | 229289270 |
| rehydrate rejects | `session_mismatch`, `bad_timestamp`, `too_old` (> 604,800,000 ms = 7 days), `already_compacted`, `boundary_missing`, `shrank_too_much` (context now < half of `preCompactTokens`), `preserve_uuid_missing`, `rows_unreadable` | 229286995 |
| reuse refusals at swap | `boundary_uuid_missing`, `covered_messages_changed`, `kept_message_missing`, `unseen_reply`, `hook_rewrite`, `would_not_fit` | 217138401 |
| hooks | PreCompact runs with trigger `precompute` and can block it ("Precomputed compact blocked by PreCompact hook") | 229292065 |
| plugin event | `session.compact`: `{trigger, agentId, instructions}` in, `{messages, tokensBefore, tokensAfter, usage}` or `{skip: reason}` out, with `trigger:"precompute"` handled; telemetry `tengu_compact_replaced_by_hook` | 221411439 |
| failure cap | three consecutive counted failures stop arming (`nsn=3`) | — |

pug measured the effect: a 47-88 s stall at overflow became 31-134 ms (pug's 2.1.311 trace,
summarized in the brief). The `session.compact` plugin event answers most of cv's own HARNESS-ASKS
Ask 1 ("let PreCompact supply the compacted segment", docs/design/HARNESS-ASKS.md:37-60) by another
door; it is observed in the binary, not yet exercised (first task of §9 L4).

### 4b. The sidecar record (cv's, all harnesses)

| field | why |
|---|---|
| `key.checkpoint` = `(FileId, offset, codec VERSION, demand)` of the read that produced the summarized prefix | cv already proves "same prefix, grown" by content (`FileId::admits`); resuming at `offset` costs O(appended) (docs/design/COMPLEXITY.md, Claude discovery row) |
| `key.anchor` = harness record id of the last summarized message (Claude uuid; Hermes message rowid; Codex line) | the harness validates by its own id; checkpoints sample bytes, they do not prove the prefix (DATAPATHS boundary 3) |
| `preserve` = ids kept verbatim after the summary | Claude's `preserveUuids` |
| `summary` (text + structured sections, §4e) | |
| `summarizer` = `(model, endpoint, prompt_version, cv_version)` | an external query's answer must be recorded, not recomputed (Fowler, "External Queries", pb/fowler2005-docs-event-sourcing.txt:269-277) |
| `tokens_before`, `tokens_summary`, `ready_ms`, `attempt`, `created_hlc` | validation and §8 |
| `parent_sidecar` | incremental extension (§4d) |

Why a checkpoint and not a hash of the IR: the IR's bytes change whenever cv's rendering changes
(every adapter fix moves `dump-hash.sh` lines), hashing it costs O(s) per check, and it says
nothing about the harness's own ids, which are what the swap must match. Caveat from
DATAPATHS boundary 1: "physical append does not imply logical append" (Gemini rewind, OpenClaw leaf
changes, Kimi usage revision); for those adapters the key is the anchor id plus the checkpoint of
the last *finalized* record, and a sidecar is never offered for an unsettled tail.

### 4c. Trigger, ownership, handoff

Trigger: fill fraction (Claude's formula above; Hermes `threshold_percent` 0.75,
`H/agent/context_engine.py:68`) and idle. Idle precompute is Letta's "sleep-time compute" (Lin et al.
2025, arxiv.org/abs/2504.13171). The trigger runs in `cv watch` from ledger `model_call` rows
(token usage), so a session that never overflows costs one summary at most.

| path | Claude Code | Hermes | lesson that applies |
|---|---|---|---|
| **(b) in-process (first)** | `session.compact` plugin returns `{messages}` built from the sidecar; PreCompact hook only to observe/veto | `register_context_engine` (`H/hermes_cli/plugins.py:686`): a `ContextEngine` whose `compress()` (`context_engine.py:98`) returns the sidecar's OpenAI-format list and whose `on_turn_complete` (:143) arms the precompute | no new network hop, nothing to fail open |
| **(a) proxy (pug owns)** | `ANTHROPIC_BASE_URL` | provider `base_url` | must fail open: "a failing/overloaded proxy is going to strand your team" while best-effort OTel loses only traces (HN 50151130) |
| **(c) session swap (last)** | new session from summary + preserved tail through cv's emitter (`distill`, `resume`) | `--resume` a new id | a swap is a new stream: record `parent` in the ledger or the lineage is lost |

Ownership as pug proposed: the proxy intercepts; cv stores, validates and swaps. In paths (b) and (c)
cv also serves the summary.

### 4d. Event-sourcing lessons applied

| lesson | source | rule for precompact |
|---|---|---|
| a snapshot is "a denormalization of the current state ... at a given point in time", "just a heuristic"; the stream is still the truth | Young, CQRS Documents, pb/young2010-cqrs-documents.txt:1030-1060 | the transcript is never edited or truncated by cv; the sidecar is a cache |
| snapshot schema = the aggregate's code at write time; changing the code silently invalidates old snapshots | appscale.blog event-sourcing in production (2026) | sidecar has `version`; mismatch = discard, never migrate (Claude does the same: `version` 5, `cliVersion` recorded) |
| external effects are not re-performed by replay | Fowler, pb/fowler2005:269-277 | rebuilding a sidecar never re-calls a model unless asked; the stored summary is the record |
| after compaction, offsets stay valid positions | Kafka, pb/kafka2026-docs-design.md:409 | cuts are named by original record id/offset, never by position in the compacted context |
| incremental summaries drift unless anchored | Factory, "Compressing Context" and "Evaluating Context Compression" (factory.com/news/compressing-context, /evaluating-compression): each summary is anchored to a message and new spans merge into the persisted summary | extension: summarize only `(anchor, new anchor]` and merge into `parent_sidecar`'s sections; full regeneration only on validation failure |
| recursive summarization of evicted messages | MemGPT (Packer et al. 2023, arXiv 2310.08560, OA W4387636003); Letta docs, letta.com/blog/agent-memory | borrow: summary-of-summary with the queue's ids; recall storage stays queryable (here: the transcript through `cv show`) |
| **must not** borrow from MemGPT | same | self-editing memory through model tool calls is nondeterministic and unvalidated; the swap path takes only a validated sidecar |

### 4e. Summary format and validation

Format: fixed sections (goal and intent; decisions with reasons; files touched; commands and
commit shas; open questions; task/lane ids; next step), every identifier verbatim, plus an
`anchors` list of message ids each section cites, so `cv show <id> --around <anchor>` expands any
line. This is cv's HARNESS-ASKS Ask 1 rendering (docs/design/HARNESS-ASKS.md:56-60). Emitted per
harness as its native form (Claude `summaryMessages`; Hermes an OpenAI-format list).

Validation at handoff, all required: Claude's eight rehydrate checks (§4a) generalized; the checkpoint
admits the current file; every `preserve` id present; tokens after swap fit (`would_not_fit`); age
under the harness's limit. Quality is measured, not asserted: Factory's probe method (questions
whose answers were in the dropped span, asked after compaction) per §8. Long-context degradation
is why this matters at all (Chroma, "Context Rot", trychroma.com/research/context-rot).

## 5. The host dimension in the IR, and the one breaking JSON change

**Verdict: `host` becomes a required field of `SessionRef` and of every JSON session reference;
`host:` is a selector; this is 0.19's one break.**

| change | where | note |
|---|---|---|
| `SessionRef.host: HostId` | `cv-core/src/ir.rs:1136` | local sessions get the local host id, never null |
| JSON: every object naming a session gains `"host"`; the session ref string becomes `host/harness/id` where a string is used | `cv schema`, MCP, HTTP | the break; old consumers keyed on `id` alone collide across hosts |
| selector `host:<name>` | the one API's selectors (docs/INTERFACE-V2.md "The shape") | a selector, not a command |
| catalog key `(host, harness, id)` | catalog DB | one catalog bump (the 0.18 rule: one bump per release, PLAN-0.18 I1) |
| `path` stays host-relative | — | §3d session-roam lesson |

Per the stability page (0.18 Lane 0), the break ships with a schema-diff entry and a removed-name
pointer for any renamed key (docs/INTERFACE-V2.md §1: removed names exit 2 with a pointer).

## 6. The drift problem

**Verdict: record from contracts, read stores as evidence, and extend the corpus method to hook,
plugin and OTel payloads.**

| surface | observed drift | source | consequence |
|---|---|---|---|
| Claude store | ~1 consequential change per 1-2 releases, releases ~daily | INJ §6 | renders degrade; the record does not |
| Claude hooks | 33 events, same list 2.1.305-310 | INJ §6 | stable contract |
| Claude OTel | roster unchanged 305-310 | INJ §6 | stable contract |
| Hermes store | ~1 schema bump per 1-2 weeks (26→30→31) | INJ §6 | evidence only |
| Hermes hooks | +7 events, 0 removed | INJ §6 | additive |
| OTel GenAI conventions | "Status: Development", moved to their own repo | github.com/open-telemetry/semantic-conventions-genai | never normalize at ingest |
| competing conventions | OTel GenAI vs OpenInference, both in production | HN 47074449 | store raw + `payload_v` |

Method: the corpus (`crates/cv-core/tests/corpus/<harness>/<version>/`, never regenerated in place)
gains `hooks/` and `otel/` captures per harness version, scrubbed, golden-tested against the
normalizer; `cv doctor`'s drift row (0.18: writer version seen vs corpus-verified) adds "payload
version seen vs captured". A new payload version is recorded raw and flagged, never dropped.

## 7. What the state of the art says will bite

| risk | who learned it | mitigation chosen |
|---|---|---|
| **proxy as single point of failure** | HN 50151130 | in-process handoff first; proxy fails open; ledger never depends on the proxy |
| **silent hook failure** | INJ §5: `cv hook` exits 0 on error (`cv/src/cmd/hooks.rs:991-998`); Hermes skips unconsented shell hooks and only logs | heartbeat cross-check: every store session must have a `birth` row; `doctor` reports a `gap` row per miss; plugins over shell hooks for Hermes |
| **lost tail on crash** | INJ §2: no SessionEnd on SIGKILL; Hermes `ended_at` NULL forever; OTel batch in flight lost | launcher records `exit`; without a launcher, `doctor` infers "ended without end" from a dead pid in `sessions/<pid>.json` |
| **backpressure / drops** | OTel Collector: "If data cannot be added to the sending queue, it is typically dropped"; persistent queue via `file_storage` (opentelemetry.io/docs/collector/resiliency; exporterhelper README) | the hook appends locally and returns; shipping is a separate cursor over the file. Hub down = ledger grows locally; disk pressure = stop and report (ember's rule) |
| **ingest lag** | Langfuse: "a few minute gap before ... traces being available" (HN 50270795) | the hub reads the mirrored suffix directly; no ingest queue between append and read |
| **storage blow-up** | Langfuse's ClickHouse at terabyte scale on a MacBook (HN 49661637); ClickHouse replaced OTel JSON logs, 8k CPUs to 90 (HN 44336015, 44336842) | raw rows are small; transcript bytes stay where they are and mirror lazily; no column store in 0.19 |
| **schema-migration pain** | Langfuse v2→v3 "painful migration" (HN 46656836) | rows are versioned facts; derived tables rebuild from the ledger; no in-place migration |
| **cardinality** | wide-events practice (Scuba lineage): high-cardinality ids as fields of one wide event (HN 39529775, 42497338) | cv emits no metrics; session/host/lane are row fields |
| **sampling** | Dapper samples (1/1024 at Google) because volume forced it | cv records everything: 3-7k rows/day/fleet; sampling would lose the one crash row |
| **clock skew** | Jepsen Cassandra; Dapper | HLC + causal edges (§2) |
| **retention deletes evidence** | INJ §2: `cleanupPeriodDays` deletes transcripts; Hermes WAL capped 64 MiB | ledger rows survive; `doctor` flags hosts with default retention |
| **cloud-only tooling** | "LangSmith needs a cloud account to see my own traces" (HN 48063536) | local-first ledger; hub optional |
| **O(n²) agent chatter** | "cost grows with the square of the agent count" (HN 50048471) | the ledger is a broadcast log; seats read by cursor, never talk pairwise |

## 8. Measurement plan on pug's fleet

Per seat, before and after, the way pug measured the stall (interleaved runs, load stated, work
counts preferred: docs/design/PERF.md "How to measure without lying").

| # | measure | how | pass when |
|---|---|---|---|
| M1 | compaction stall, per harness | time from overflow to the next request leaving (proxy log or ledger `compact` → `model_call`) | Claude reproduces pug's 47-88 s → 31-134 ms; Hermes shows a comparable drop |
| M2 | hook/plugin latency added | per event p50/p99 from ledger `wall` vs harness timestamps | within 0.18's measured per-prompt hook budget (< 40 ms, memory `cv-0-18-plan.md` Lane M) |
| M3 | ledger volume | rows and bytes per seat-day | stated; no target |
| M4 | hub lag | append → visible at hub, p50/p95 | p95 ≤ 2 s (helm's falsifier) |
| M5 | record completeness | store sessions with a `birth` row / all store sessions; `gap` rows per day | every gap explained |
| M6 | sidecar outcomes | ready / consumed / rejected by reason / discarded | reasons distribution stated |
| M7 | summary quality | Factory-style probes on 20 real compactions per harness, cv summary vs harness default | cv ≥ default |
| M8 | Hermes reader under 50 writers | read failures, copies, O(store) re-walks per query | failures 0; one write re-reads one session |
| M9 | crash truth | launched sessions killed with SIGKILL: `exit` rows / kills | 100% with launcher |

## 9. Order of building

ember's rule from fn2/FN2-ORIENTATION.md: bottom-up, each layer one typed boundary settled before
the next, and "a tree that does not serve is a design doc". Week one serves on one host.

| layer | builds | serves from day one |
|---|---|---|
| L0 identity + ledger | `HostId`, `(host, incarnation, seq, hlc)`, the append/read/wait/ack trait, file provider | `cv ls` shows `host`; `cv events` reads the ledger |
| L1 recorder | Claude: all hook events as async command handlers appending rows; OTel env via managed settings (`managed-settings.d/cv.json`, INJ §7b); Hermes plugin (§3e.1) | every live session has a `birth` row; `doctor` gap row |
| L2 Hermes reader fix | §3e.2 | 50 writers, zero read failures |
| L3 hub | `cv serve` hub routes, mirror-by-suffix shipper, long-poll `wait`, G-Set merge | `cv ls --all-hosts` on the hub |
| L4 precompact | verify `session.compact` by a real plugin; sidecar + validation; Hermes `ContextEngine`; then pug's proxy | M1, M6 |
| L5 launcher | wrapper: id at birth, env stamping, request tag, exit/signal | M9 |
| L6 fn provider | when fn serves remote wait/ack | same port, other provider |

## 10. Open decisions

| # | decision | recommendation |
|---|---|---|
| D1 | mirror-by-suffix vs federation | mirror the ledger; federate only cold transcript fetches |
| D2 | `host` as a required JSON field (the break) | yes, in 0.19, once |
| D3 | hub = `cv serve` mode or a separate daemon | `cv serve` mode (the shape forbids a new server) |
| D4 | how precompact is exposed under "never a new top-level command" | a `cv watch` consumer + `--kind precompact` on `show`/API; no `cv precompact` verb |
| D5 | launcher verb | extend `resume` (it already "launches an existing session") to launch a new one, or a hidden wrapper; decide with ember |
| D6 | HLC vs Lamport | HLC |
| D7 | ledger durability | fsync per batch with group commit; rows lost in a crash between append and fsync are counted by `seq` gaps |
| D8 | in-process before proxy for precompact | yes; proxy fails open |
| D9 | summary format | structured sections + anchors (§4e); harness-native emission |
| D10 | fn as provider | define the port now, adopt fn when remote wait/ack serves; do not wait for it |
| D11 | OTel normalization | store raw, normalize at read; track GenAI semconv, do not depend on it |
| D12 | Hermes upstream asks | ask for `on_session_start` at launch and a pid/host column; until then the launcher covers birth |
