# Harnesses & where they store sessions

clustervision currently understands **21 harnesses** 🔮 (incl. the ChatGPT & Claude.ai account data
exports — `chatgpt-export`/`claude-export`, register via `cv config --add-export`). Every one of them couples a session to a
working directory — they encode the `cwd` into the path, the filename, a hash, or a DB column — which
is exactly why your sessions feel "dir-jailed" and impossible to find later. clustervision reads them
all into one unified IR so that coupling stops mattering.

This chapter is the field guide: for each harness, *where* it keeps sessions on disk, whether we can
**parse** it (read it into the IR) and whether we can **emit** it (write the IR back out, making it a
[conversion](conversion.md) target).

The ground-truth catalog these adapters are built from lives in
[`docs/FORMATS.md`](https://github.com/emberian/cv/blob/main/docs/FORMATS.md), reverse-engineered
from a real machine. The table below is the centerpiece; per-harness notes follow.

## The master table

All paths are the real ones the adapters look at. `~` is your home dir; `<AppSupport>` is
`~/Library/Application Support` on macOS, `~/.config` on Linux, `%APPDATA%` on Windows.

| Harness | On-disk location | Parse | Emit | Notes |
|---|---|:--:|:--:|---|
| **Claude Code** | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | ✓ | ✓ | One JSONL per session; threaded via `uuid`/`parentUuid`. **Sub-agents** under `<sid>/subagents/agent-*.jsonl`. Dir-name encoding is lossy — `cwd` is read from inside. Also reads `$CLAUDE_CONFIG_DIR` and extra roots — see [below](#claude-code-sessions-outside-claude). |
| **Codex CLI** | `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<ULID>.jsonl` | ✓ | ✓ | Also `~/.codex/archived_sessions/`. **SQLite index** `state_5.sqlite`. 2025 **legacy** variant = single `{session,items[]}` JSON. |
| **Grok CLI** | `~/.grok/sessions/<percent-encoded-cwd>/<sid>/` | ✓ | ✓ | A *directory* per session (`chat_history.jsonl` + sidecars). **Sub-agents** flagged by `session_kind:"subagent"`. cwd is reversible (`%2F`). |
| **OpenCode** | `~/.local/share/opencode/storage/` | ✓ | ✓ | `session/**/ses_*.json` + `message/<sid>/*.json` + `part/<msgid>/*.json`. **Subtask** parts spawn sub-sessions. Two storage generations (inline-summary vs parts). |
| **OpenClaw** | `~/.openclaw/agents/<agentId>/sessions/<sid>.jsonl` | ✓ | ✓ | `sessions.json` index + JSONL transcripts. Header `version` v1/v2 linear → **v3 parent-linked** (migrated in place). ACP-bridged sessions are text-only echoes. |
| **Gemini / Antigravity** | `~/.gemini/tmp/<projectHash>/chats/session-*.json\|jsonl` | ✓ | ✓ | Multi-format, best-effort. Rich chat recordings + `checkpoint-*.json` + `logs.json` fallback. **Sub-agents** nested at `chats/<parentId>/<id>.jsonl`. Antigravity `*.pb` protobufs are opaque (not parsed). |
| **Hermes (Nous)** | `~/.hermes/state.db` (**SQLite**) | ✓ | ✓ | One DB, all sessions; `$HERMES_HOME` overrides. cwd is *not* persisted. Schema drifts live (probe `PRAGMA table_info`). Compression chains linked by `parent_session_id`. |
| **Kimi CLI** | `~/.kimi/sessions/<md5(cwd)>/<uuid>/context.jsonl` | ✓ | — | Store **frozen since 2026-06** (kimi-cli migrates it to `~/.kimi-code` on open), so it stopped being a port target in 0.14. `$KIMI_SHARE_DIR` overrides. Project dir = `md5(cwd)`; cwd recovered from `kimi.json` `work_dirs[]`. **Legacy** flat form `<hash>/<uuid>.jsonl`. `wire.jsonl` sidecar enriches. |
| **Kimi Code** | `~/.kimi-code/sessions/wd_<slug>_<sha256(cwd)[:12]>/session_<uuid>/` | ✓ | — | Moonshot's successor to kimi-cli; `$KIMI_CODE_HOME` overrides. `state.json` stores cwd, title and the agent map; one `agents/<agentId>/wire.jsonl` per agent (`main` + `Agent`-tool sub-agents); oversized outputs in `tool-results/`. |
| **LM Studio** | `~/.lmstudio/conversations/<ms-epoch>.conversation.json` | ✓ | ✓ | Plaintext JSON, one file per chat. No cwd (chat app). Keeps every regenerated variant; we read the selected one. No first-class tool calls. |
| **Cline** | `<AppSupport>/<Editor>/User/globalStorage/saoudrizwan.claude-dev/tasks/<taskId>/` | ✓ | ✓ | VS Code extension; `api_conversation_history.json` is a raw Anthropic-Messages array (≈1:1 with Claude). Also `~/.cline/tasks/`. cwd from first user msg's `<environment_details>`. |
| **Roo Code** | `<AppSupport>/<Editor>/User/globalStorage/rooveterinaryinc.roo-cline\|roo-code/tasks/<taskId>/` | ✓ | ✓ | A **Cline fork** — identical per-task layout, different namespace. Also `~/.roo/tasks/`. Reuses the Cline parser/emitter, tagged `Harness::Roo`. |
| **Continue (continue.dev)** | `~/.continue/sessions/<sid>.json` | ✓ | ✓ | `$CONTINUE_GLOBAL_DIR` overrides. `sessions.json` index + per-session file. OpenAI-shaped messages; cwd = `workspaceDirectory`. |
| **Qwen Code** | `~/.qwen/projects/<cwd>/chats/<id>.jsonl` (+ legacy `~/.qwen/tmp/<hash>/chats/session-*.json\|jsonl`) | ✓ | ✓ | Claude-Code-shaped records carrying Gemini parts; sub-agents under `subagents/<parent>/agent-<id>.jsonl`. The legacy gemini-cli shape delegates to the Gemini parser. |
| **Cursor IDE** | `<AppSupport>/Cursor/User/globalStorage/state.vscdb` (**SQLite**) | ✓ | — | Closed-source, opened read-only. Global DB holds `composerData:`/`bubbleId:` rows; per-workspace DB links composers → cwd. Schema churns (`_v` 1..=10+). |
| **Goose (Block)** | `<AppSupport>/Block.block.goose/sessions/sessions.db` (**SQLite**) | ✓ | — | `$GOOSE_PATH_ROOT`/`$XDG_DATA_HOME` override. Modern `sessions.db` + **legacy** per-session `<name>.jsonl`. MCP-style tool content; read-only, PRAGMA-probed. |
| **Zed (agent panel)** | `<data_dir>/Zed/threads/threads.db` (**SQLite**) | ✓ | — | One DB, one row per thread; `data` BLOB = **zstd-compressed JSON** (libzstd on native builds, pure-Rust `ruzstd` on wasm). Three blob generations (versionless / agent1 0.1–0.2 / agent2 0.3 tagged-enum). cwd + git from the project snapshot; **subagents** linked by `parent_id`. |
| **Devin CLI** (Cognition) | `~/.local/share/devin/cli/sessions.db` (**SQLite**) | ✓ | — | Closed-source, opened read-only (the CLI keeps writing while we read). `message_nodes` is a **forest** of re-prefixed copies — the transcript is the `main_chain_id`→root walk. Thinking + parallel tool calls + `sealed` signatures preserved. |
| **ChatGPT export** | a `conversations.json` you register with `cv config --add-export <path>` | ✓ | — | The "Export data" archive. One file = many conversations; each is a `mapping` DAG (we walk root→`current_node`). Opt-in (no fixed home); deduped by id. |
| **OpenSession** | `*.opensession.json` (or a `*.json` with an `open_session` key) under a dir you register with `cv config --add-export <path>` | ✓ | — | cv's own interchange format, written by `cv export --format json` and read back in. 0.3 *is* the IR; a 0.2 document is refused with a pointer. See [OpenSession](conversion.md#the-opensession-standard). |
| **Claude.ai export** | a `conversations.json` you register with `cv config --add-export <path>` | ✓ | — | The claude.ai "Export data" archive. One file = many conversations; linear `chat_messages[]` (text/thinking/tool_use/tool_result). Opt-in; deduped by id. |

`✓` = supported · `—` = not supported.

**12 harnesses are emit-capable** (conversion targets): Claude Code, Codex, Grok, OpenCode, OpenClaw,
Gemini, Hermes, LM Studio, Cline, Roo, Continue, Qwen. The authoritative list is
`supported_targets()` in `emit.rs`.

**9 are parse-only**: Cursor, Goose, Zed, Devin CLI and Kimi Code (closed-source / read-only stores we
deliberately don't write back to), Kimi CLI (a frozen store), OpenSession (an export, not a harness
to resume in), and the two account-data **exports** (`chatgpt-export` / `claude-export`, registered
via `cv config --add-export`).

Claude Code 2.1.304 is the verified writer in the committed corpus, including request-ledger
records, fork sub-agents, retries and the harness's `--max-turns` option. Unknown records are
carried in complete reads rather than counted as turns; `cv doctor` compares observed writer
versions with this corpus coverage.

## Claude Code sessions outside `~/.claude`

Claude Code keeps its sessions in `~/.claude/projects/` unless `CLAUDE_CONFIG_DIR` moves its config
dir. Tools that run many Claude Code agents often give each agent its own config dir, so one machine
can hold sessions in many places. clustervision reads all of them, in this order:

1. `$CLAUDE_CONFIG_DIR/projects`, when `CLAUDE_CONFIG_DIR` is set. This is where the Claude Code of
   the current environment writes, so it is also where `cv port --harness claude` writes when you
   do not give `--out`.
2. `~/.claude/projects`.
3. Each entry of `CLUSTERVISION_CLAUDE_ROOTS`, a list separated like `PATH` (`:` on Unix, `;` on
   Windows).
4. Each line of `$CLUSTERVISION_HOME/claude-roots` (by default `~/.clustervision/claude-roots`).
   Blank lines and lines that start with `#` are ignored.

An entry can name a Claude config dir (a directory that contains `projects/`) or a `projects/`
directory itself, and it can start with `~`. A path segment that is exactly `*` matches each entry
of the directory before it, and an entry can have more than one. For example, these lines in
`~/.clustervision/claude-roots` read every agent that has its own config dir under `~/.agents/`,
and every instance of an agent that runs several:

```text
~/.agents/*/claude
~/.agents/*/instances/*/claude
```

cv expands each `*` every time it looks for sessions, so it finds an agent that you add later
without a restart. Entries that do not exist are ignored. A directory that you name more than once
(for example, through a symlink) is read once. A config dir that does not contain `projects/` yet has
no sessions, and cv skips it until `projects/` appears.

Prefer the file to the environment variable. All cv processes on the machine (the CLI, each MCP
server, the daemon) share one catalog and read the same file, so they agree about which sessions
exist. An environment variable applies only to the processes that have it.

If two roots hold the same session id (for example, a transcript that was copied to a second config
dir and continued there), `cv ls` shows both files, and `cv show <id>` opens the copy that was
updated most recently.

## Sub-agents

Several harnesses spawn child agents, and clustervision surfaces those as nested sessions you can browse
as a tree — see [sub-agent trees in the app](viewing-and-sharing.md).

- **Claude Code** writes Task sub-agents to a sibling `<sid>/subagents/agent-<id>.jsonl` (each with a
  `.meta.json` carrying `agentType` and `description`). The discovery walk is depth-limited so these
  don't get mistaken for top-level sessions; they're attached to their parent instead.
- **Kimi** keeps each sub-agent in `<session>/subagents/<agent_id>/context.jsonl`, **Kimi Code** in
  `<session>/agents/agent-N/wire.jsonl`, and **Hermes** marks a delegate run with
  `model_config._delegate_from`; discovery leaves them out and each parent's forest lists them, as
  `<parent>/<agent>` for the two Kimis.
- **Grok** tags subagent sessions with `session_kind: "subagent"` (primary sessions instead carry an
  `agent_name`); the kind is preserved in the message `extra`.
- **Gemini** dir-nests subagent recordings under `chats/<parentId>/<id>.jsonl`; **Qwen** writes them to `subagents/<parentId>/agent-<id>.jsonl`.
- **OpenCode** models them as `subtask` parts — a spawned sub-session with its own prompt, agent, and
  model — alongside `agent` mention parts.
- **Zed** stores a subagent thread as its own row in `threads.db` with a `parent_id` column (and, in
  the 0.3.0 blob, a `subagent_context{parent_thread_id, depth}`); we keep the link in
  `Session::lineage.parent` (with `depth` in `extra["zed"]`).

## The SQLite stores

Five harnesses keep everything in a SQLite database rather than per-session files: **Hermes**
(`state.db`), **Cursor** (`state.vscdb`), **Goose** (`sessions.db`), **Zed** (`threads.db`, whose
rows are additionally zstd-compressed JSON blobs) and **Devin** (`sessions.db`); OpenCode 2.x and
OpenClaw's newer stores are SQLite too, beside their file formats. All are opened **read-only**
(a WAL database through a private snapshot, so cv never creates or rewrites a harness's `-wal`/`-shm`),
every cell is read once through `store::sqlite::Cells`, and because their schemas accrete columns
across versions without a clean version gate, each adapter probes `PRAGMA table_info` and selects
only the columns that actually exist — so an older database degrades gracefully instead of failing
the whole import. (Codex and Grok also ship SQLite, but only as *indexes* alongside their JSONL
transcripts, not as the primary store.)

## The two desktop apps are not harnesses

The Claude desktop app keeps no transcripts on disk (claude.ai holds them server-side; the Claude Code
sessions it launches are ordinary `claude` sessions), and the ChatGPT desktop app encrypts its history at
rest with a key cv cannot read. Both used to be listed as `claude-app`/`chatgpt-app` with an always-empty
`discover()`; those names now fail with a pointer (exit 2). The findings are kept in
[`docs/FORMATS.md`](https://github.com/emberian/cv/blob/main/docs/FORMATS.md) under "Desktop apps".

## Bring your own harness

Don't see your agent here? Adding one is a single new module implementing the `Adapter` trait —
`discover` + `parse`, and an optional `emit` to make it a conversion target. The full walkthrough is in
[Adding a harness](adding-a-harness.md).
