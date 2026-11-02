# Injection surfaces: Claude Code and Hermes (cv 0.19 design investigation, 2026-10-24)

The question: can cv attach itself to Claude Code and Hermes so that nothing the harness does is
hidden from it, without changing how the user launches agents? Or is that whack-a-mole, one hook
and one data-loss shape at a time?

The target case: 50 Hermes sessions running at once against a local inference cluster, plus Claude
Code sessions, spread over about 12 machines. cv is the system of record: it assigns identity at
birth, records events as they happen, and keeps transcripts as evidence.

**Ground truth used.** Claude Code: the installed binaries 2.1.305 to 2.1.310 under
`~/.local/share/claude/versions/`, read with grep and Python mmap (there is no `~/pug/claude-code*`
checkout). Hermes: the installed 0.21.6 package at
`/opt/homebrew/Cellar/hermes-agent/0.21.6/libexec/lib/python3.14/site-packages` (written below as `H/`),
the `~/pug/hermes-agent` checkout (HEAD `a12c3417c8`, 759 commits past v0.21.6), and a copy of
`~/.hermes/state.db` (schema 30, last written 09-26) in the scratchpad, since deleted. cv: main at
`915ace06`. No model calls were made and no real home was written.

## 1. Claude Code: surfaces

| surface | what it is | how cv uses it today |
|---|---|---|
| **store** | `projects/<cwd>/<sid>.jsonl`, `<sid>/subagents/agent-*.jsonl` + meta, `subagents/workflows/<run>/journal.jsonl`, `workflows/wf_*.json`, `<sid>/tool-results/*.txt`; the request ledger (`api-request`, `-shape`, `-blob`, from 2.1.300); `sessions/<pid>.json` (live registry, deleted at exit); `history.jsonl`; `tasks/`, `file-history/`; `$CLAUDE_CODE_TMPDIR` | read by the adapter (docs/FORMATS.md:65-282) |
| **hooks** | 33 events in 2.1.310 (the same list in 305-310): PreToolUse, PostToolUse, PostToolUseFailure, PostToolBatch, Notification, UserPromptSubmit, UserPromptExpansion, SessionStart, SessionEnd, Stop, StopFailure, SubagentStart, SubagentStop, PreCompact, PostCompact, Pre/PostModelSwitch, PermissionRequest, PermissionDenied, Setup, TeammateIdle, TaskCreated, TaskCompleted, Elicitation, ElicitationResult, ConfigChange, WorktreeCreate/Remove, InstructionsLoaded, CwdChanged, FileChanged, DirectoryAdded, **MessageDisplay** (streamed assistant text, one call per flush). Handler types include `http` (`allowedHttpHookUrls`) | 5 hooks, 6 entries (crates/cv/src/cmd/hooks.rs:132-171): SessionStart(compact), SessionStart(startup\|resume), SubagentStart/Stop (async), UserPromptSubmit, PreToolUse(Edit\|Write) |
| **OTel** | `CLAUDE_CODE_ENABLE_TELEMETRY` + `OTEL_*`. Log events: user_prompt, api_request (`ttft_ms`, duration), api_error, api_retries_exhausted, api_refusal, assistant_response (text redacted unless a flag is set), tool_decision, tool_result, tool, subagent_completed, compaction, system_prompt, hook_execution_start/complete, permission_mode_changed, mcp_server_connection, skill_activated, retention_sweep, …. Spans in beta tracing (`CLAUDE_CODE_ENHANCED_TELEMETRY_BETA`): llm_request, tool, tool.execution, mcp.rpc, subagent.spawn, compaction, hook, interaction. Raw request/response bodies with `OTEL_LOG_RAW_API_BODIES`. `OTEL_RESOURCE_ATTRIBUTES` stamps any tags | **unused** |
| **settings layers** | user `~/.claude/settings.json`, project `.claude/settings.json`, local `settings.local.json`, `--settings`, **managed** `/Library/Application Support/ClaudeCode/managed-settings.json` + `managed-settings.d/` (macOS), `/etc/claude-code/` (Linux), with `allowManagedHooksOnly` and `disableAllHooks`. The settings `env` block sets environment variables, including the OTel ones | cv writes user or project scope only (hooks.rs:207-215, 271) |
| **MCP** | cv as a server sees only the calls made to it | not used |
| **env / CLI** | `--session-id <uuid>`, `-n`, `CLAUDE_CONFIG_DIR`, `CLAUDE_ENV_FILE` (SessionStart exports env to later Bash calls), `CLAUDE_CODE_SESSION_ID` in children, `ANTHROPIC_CUSTOM_HEADERS` | `CLAUDE_CONFIG_DIR` roots are read |
| **network** | `ANTHROPIC_BASE_URL` proxy; requests carry `X-Claude-Code-Session-Id` | none (the corpus proxy only) |

Common hook fields (binary schema `z`): `session_id`, `transcript_path`, `cwd`, `prompt_id` ("same
value emitted on OpenTelemetry events as `prompt.id`"), `permission_mode`, `agent_id`, `agent_type`.

## 2. Claude Code: lifecycle × surface

Latency: hooks are synchronous unless `async`, and fire in process. The store is a JSONL append with
no per-write fsync check. OTel is batched (`OTEL_BLRP_SCHEDULE_DELAY`).

| moment | store | hooks | OTel | cannot see |
|---|---|---|---|---|
| launch | `sessions/<pid>.json` at start (pid, sessionId, cwd, version, kind, entrypoint), deleted at exit; the transcript file appears only at the first record | SessionStart `source=startup`, plus `model`, `session_title` | session.count metric | intent, lane, host; the parent when it is another host |
| identity | `sessionId` inside every record | `session_id` | `session.id` | a launcher-chosen id, unless launched with `--session-id` |
| prompt | `user` record, `promptId`, `origin`, `turnOrigin` | UserPromptSubmit `prompt`, `source` (user\|sdk\|system\|loop_wakeup\|schedule_wakeup\|poll_event); UserPromptExpansion | user_prompt (text opt-in) | nothing material |
| model call | request ledger (params, cache points, digest, full request bytes); `assistant.requestRef`; usage | none per request; MessageDisplay sees streamed text as displayed | api_request (`ttft_ms`, duration, tokens, cost), raw bodies opt-in, llm_request span | queue or server-side latency (only the proxy and the server see them) |
| tool call / result | tool_use, tool_result, `toolUseResult` sidecar, `tool-results/*.txt` | PreToolUse, PostToolUse(`tool_response`), PostToolUseFailure, PostToolBatch | tool_decision, tool_result, tool span | a large result (the hook gets it whole; the transcript keeps a stub) |
| permission | `permissionMode`; "denied" tool_result | PermissionRequest, PermissionDenied | tool_decision, permission_mode_changed | — |
| sub-agent spawn/return | `subagents/` + meta (`toolUseId`, `spawnDepth`, `isFork`), task-notification turn | SubagentStart(`agent_id`, `agent_type`), SubagentStop(`agent_transcript_path`, `last_assistant_message`); tool hooks fire inside sub-agents with `agent_id` | subagent.spawn span, subagent_completed | which compaction belongs to which agent (docs/design/HARNESS-ASKS.md); not verified: hooks inside Workflow agents |
| workflow | `wf_*.json`, `journal.jsonl`, script | none of its own | none found | the phase tree before the run file is written |
| compaction | `compact_boundary` + summary record | PreCompact (trigger, instructions; can veto), PostCompact(`compact_summary`), SessionStart `source=compact` | compaction event and span | the dropped span is still in the file, so nothing is lost |
| error / retry | `system/api_error` per retry, synthetic assistant turn | StopFailure(`error`, `error_details`) | api_error, api_retries_exhausted | — |
| abort / crash | interrupt marker turn; on SIGKILL the tail may be lost (not verified) | **nothing on SIGKILL**: no SessionEnd | the batch in flight is lost | a crash. The only trace is the registry file `sessions/<pid>.json` left behind for a dead pid |
| end | `last-prompt`, `cost-state` | SessionEnd `reason` (clear\|resume\|logout\|prompt_input_exit\|other), Stop | — | — |
| resume / fork | same file, or a new id with no back pointer (fork) | SessionStart `source=resume\|fork\|clear` | — | the fork's source id. cv infers it from shared uuids (FORMATS.md:236-242) |
| retention | — | — | retention_sweep | **`cleanupPeriodDays` deletes transcripts** (ember sets 99999 on this host; another host may use the default) |

**Claude verdict.** Store + hooks + OTel cover every moment. The holes are causal: crash, intent,
cross-host parentage, cluster queue. They are not about coverage.

## 3. Hermes: surfaces (0.21.6)

| surface | what it is | how cv uses it today |
|---|---|---|
| **store** | `$HERMES_HOME/state.db` (or per profile `profiles/<n>/state.db`), schema 31 (`H/hermes_state_common.py:283`), WAL. Tables: `sessions`, `messages`, `system_prompts`, `session_model_usage` (per model, `billing_base_url`, task: calls, tokens, cost, first_seen, last_seen), `async_delegations` (owner_pid), `session_turn_leases`, `compression_locks`, `state_meta`, FTS. `sessions/request_dump_*.json` (debug only) | adapter `crates/cv-core/src/harness/hermes.rs` |
| **plugin hooks** (in-process Python) | 44 events (`H/hermes_cli/plugins.py:109-205`). The relevant ones: pre/post_tool_call, transform_tool_result, pre/post_llm_call, **pre/post_api_request** (`base_url`, `provider`, `model`, `api_duration`, `first_chunk_at` = TTFB, usage, `response_model`; `H/agent/turn_response_intake.py:58-97`), api_request_error, pre/post_auxiliary_call (titling and compression LLM calls), on_stream_start/delta/end, on_session_start/end/finalize/reset, subagent_start/stop (`parent_session_id`, `child_summary`, `tool_call_history`, `duration_ms`), agent_loop_stopped, approval and human-input observers, kanban worker spawned/exited. LLM-request **middleware** can change the outbound payload (`H/hermes_cli/middleware.py:24`). Loading needs a `plugins.enabled` opt-in (`plugins_discovery.py:257-290`) | **none**: `cv hooks` has no Hermes host (hooks.rs:230) |
| **shell hooks** | `hooks:` in config.yaml, the same events except `transform_api_error_classification`; JSON on stdin; **first-use TTY consent per (event, command)**, else `--accept-hooks`, `HERMES_ACCEPT_HOOKS=1`, or `hooks_auto_accept`; a hook not on the allowlist is **skipped and only logged** (`H/agent/shell_hooks.py:141-165`); fails open | none |
| **OTel** | `monitoring.export.otlp`, gateway monitoring events only (`H/agent/monitoring/otlp_exporter.py:1-7`); a bundled Langfuse plugin | none |
| **env / CLI** | `HERMES_HOME`, `HERMES_PROFILE`, `--source <tag>`, `-c <name> --create-if-missing` (a named session, created if absent), `--resume <id>`, `--pass-session-id`, `--worktree`; Hermes *exports* `HERMES_SESSION_ID` to tools (`H/agent/agent_init.py:1163-1180`) | `HERMES_HOME` |
| **network** | `base_url` per provider; `model.default_headers` and provider `extra_headers` are static; **no session id is sent upstream** on chat-completions | none |

## 4. Hermes: lifecycle × surface

| moment | store | plugin hooks | cannot see |
|---|---|---|---|
| launch | **nothing**: the `sessions` row is created lazily at first use (`H/run_agent.py:313-345`, `_ensure_db_session`) | none at launch (on_session_start fires inside the first turn, `H/agent/conversation_loop.py:856-865`) | a session that never takes a turn; pid; host |
| identity | id `YYYYMMDD_HHMMSS_<6 hex>` (`H/hermes_state_ids.py:56-60`): 24 random bits per second. No pid or hostname column anywhere | `session_id` in every payload | a launcher cannot set the id except through `-c <title> --create-if-missing` |
| prompt | `user` row at the turn boundary | pre_llm_call (can add context), pre_gateway_dispatch | context added by pre_llm_call is **not stored** (AFFORDANCES.md:32) |
| model call | `session_model_usage` aggregates per (model, base_url); `messages.token_count`, `finish_reason`; no per-call row | pre/post_api_request (endpoint, duration, TTFB, usage), on_stream_delta | queue position and serving node; per-call latency in the store |
| tool call / result | the assistant `tool_calls` row is saved **before** the tools run; tool rows `tool_call_uid` (v31) | pre/post_tool_call, transform_tool_result | — |
| sub-agent | child `sessions` row `source='subagent'`, `_delegate_from`; `async_delegations` | subagent_start/stop | a background child delivered after `/exit` (2026.8.31 corpus note) |
| compaction | in place (`active`/`compacted` flags) or a rotation (`parent_session_id`, `end_reason='compression'`) | none of its own (aux calls show `aux_task`); on rotation, on_session_start fires with the new id | — |
| error | `display_kind='failed_turn'` (0.21); 2026.8.31 writes nothing | api_request_error, transform_api_error_classification | — |
| abort | `session_persistence_failed` turn exit | agent_loop_stopped (gateway) | — |
| end | `ended_at`, `end_reason` | **on_session_end fires once per turn** (`H/agent/turn_finalizer.py:791-802`); on_session_finalize is the real end | a crash: `ended_at` stays NULL forever |
| resume / branch | same id, or `_branched_from` | on_session_reset | — |

## 5. Gap list (both harnesses)

| gap | Claude | Hermes | who can close it |
|---|---|---|---|
| birth before the first record | SessionStart hook + `sessions/<pid>.json` | **none** | the launcher (Hermes), or a Hermes upstream ask |
| intent, lane, assigner | none | none (`--source` is one free tag) | the launcher only |
| parent across hosts | none | none | the launcher (env-stamped parent id) |
| host identity | none (cv has no hostname anywhere either) | none | cv itself (the ingest side) |
| cluster facts (endpoint, TTFB, node, queue) | OTel api_request has endpoint and TTFB; no queue | post_api_request has endpoint and TTFB; the store has endpoint only; no queue | queue and node: only the cluster's own logs, joined on a stamped request id |
| crash | no SessionEnd; tail not verified | `ended_at` NULL; the last turn unflushed | process supervision (the launcher waits on the pid) |
| silent hook failure | `cv hook` prints errors to stderr and exits 0 (hooks.rs:991-998); 10 s timeout (hooks.rs:17) | an unconsented shell hook is skipped and only logged; plugins fail open | a heartbeat cross-check: a hook ledger vs the store |
| store rotation / loss | cleanupPeriodDays sweep; registry deleted at exit | WAL capped at 64 MiB (`H/hermes_state_wal.py:31`); PASSIVE checkpoint every 50 writes (`H/hermes_state.py:501`); falls back to DELETE journaling on ZFS, NFS or "disk I/O error" (wal.py:25-28) | ingest early (hooks and live tail) so the store is evidence, never the only copy |
| context injected by a hook | Claude stores it (`hook_additional_context`) | Hermes does **not** store pre_llm_call context | a Hermes plugin records it |

## 6. Drift rate (what whack-a-mole would cost)

| surface | evidence | rate |
|---|---|---|
| Claude store | corpus 2.1.296 → 2.1.304 (8 days, 8 releases): +5 record types (api-request, -shape, -blob, fork-context-ref, isolation-latch), +2 attachments, api_error retry shape, ~12 new fields, meta.json fields (FORMATS.md:254-282); async-by-default sub-agents in 296 | about 1 consequential change per 1-2 releases. Releases run ~1/day (2.1.296 on 10-11 to 2.1.310 on 10-23) |
| Claude hooks | 33 events, the same list in 2.1.305-2.1.310; cv's `hook.rs` changed 2 times since 08-24 | stable, additive, documented |
| Claude OTel | event roster unchanged 2.1.305 → 2.1.310 | stable, versioned names |
| Hermes store | schema 26 → 30 → 31 between 2026.8.31 and 0.21.6 (~6 weeks): ~15 new columns, a new table, `source` changed cli→oneshot, tool renames, `failed_turn`; `hermes.rs` had 88 commits since 08-24 (12 tagged drift) | about 1 schema bump per 1-2 weeks; append-only (ALTER ADD COLUMN) |
| Hermes hooks | 2026.8.31 → 0.21.6: +7 events (agent_loop_stopped, pre/post_auxiliary_call, human_input×2, room_member_activity, post_gateway_admission), **0 removed**; HEAD (+759 commits) changes only type spellings | additive |

The store is where the moles are. Hooks and OTel are contracts, and both grow by adding.

## 7. Verdict

**(a) Reader + hooks.**
- *Claude: about 90% of moments*, if cv adopts all 33 hooks (as async `http` or command handlers),
  **plus OTel** for model calls, which no hook covers. Today cv installs 6 entries, for lanes and
  cybernetics only, not recording. The transcript remains the evidence; hooks give timing and birth.
  Gaps: crash, intent, cross-host parent, queue.
- *Hermes: about 60% from the reader alone.* No birth, no per-call facts, no crash end, injected
  context lost. *About 85% with an in-process cv plugin* (post_api_request, on_stream_*,
  subagent_*, pre_llm_call, on_session_finalize). Two changes buy it: a config opt-in
  (`plugins.enabled`) and a plugin directory. The user's launch command stays the same.

**(b) What a launcher adds that nothing else can.**
1. Identity before the first byte, chosen by cv (Claude `--session-id <uuid>`; Hermes
   `-c <cv-id> --create-if-missing` plus `--source cv`).
2. Intent, lane, parent and host stamped into env and into OTel `OTEL_RESOURCE_ATTRIBUTES`. Hooks
   and plugins carry it into every event.
3. Crash truth: the launcher waits on the pid and records exit status and signal.
4. A per-session request tag the cluster can log (Claude `ANTHROPIC_CUSTOM_HEADERS`; Hermes through
   LLM-request middleware). Without it, queue and node facts cannot be joined to a session.

A wrapper on `PATH`, or Claude managed-settings for hooks and env, avoids changing how the user
launches. Managed settings reach a dozen machines with one file per host
(`managed-settings.d/cv.json`; `allowManagedHooksOnly` makes them unremovable).

**(c) Whack-a-mole risk.** The store is the moving target (Claude ~1 change per 1-2 releases at
~1 release/day; Hermes ~1 schema bump per 1-2 weeks). Hooks and OTel are additive contracts with
stable names across the observed span. The design lever: **record from the contracts** (hooks, OTel,
plugin), keyed by ids cv assigned, and **read the store as evidence**, linked by those ids. A
store-format change then degrades rendering, never the record of what happened. Without a launcher
the whack-a-mole stays real on one axis: birth and crash, which no harness surface covers.

**(d) 50 Hermes processes on one host: what breaks today.**
- *Identity*: no row until each session's first turn; no host or pid column. 50 ids minted in
  the same second have a 50²/2·2⁻²⁴ ≈ 7e-5 collision chance: fine.
- *Concurrent writers*: Hermes serializes with BEGIN IMMEDIATE and jittered retry (transcript
  patience 60 s, activity 0.5 s; `H/hermes_state.py:484-501`). Activity stamps (`last_activity_at`,
  which cv uses as `updated_at`) are dropped under contention.
- *cv's reader*: with WAL frames present, `open_ro` copies db + WAL per store state per operation
  (`crates/cv-core/src/harness/store/sqlite.rs:1-16,254-300`). Every read is bracketed by
  `check_reads`, and any write during it fails the read: "SQLite source dependencies changed"
  (sqlite.rs:181-186). The discovery cache, `walked()` (hermes.rs:662-684) and the snapshot are all
  keyed on db+WAL file identity, so **one write by any of the 50 invalidates every session's
  certificate and re-walks the whole `sessions` table**. Under 50 live writers this means
  continuous copies, constant read failures, and read amplification of O(store) per query.
  Per-session certificates exist (`sqlite/certify.rs`), but the store-level gate defeats them.
- *Cluster facts*: the store has `billing_base_url`, model, and per-(model, endpoint) call and
  token totals with first and last seen. No per-call latency, no node, no queue.
- **Minimum change.**
  1. A cv Hermes plugin that appends post_api_request, subagent_*, on_session_finalize and
     pre_llm_call events (with `session_id`) to a per-host append-only ledger; this is the birth
     and timing record.
  2. In cv's SQLite reader: retry a failed `check_reads`, and gate certificates on the per-session
     rowid range read from the snapshot instead of file identity. Then a write to session A leaves
     B's certificate valid.
  3. A launcher shim (or the `-c <id> --create-if-missing` convention) for identity at birth, plus
     a request tag for the cluster.
  4. A host field in cv's IR and ledger so 12 hosts' ledgers merge.
