# Cross-harness conversion

Every harness parses into one IR; conversion is parse(A), then emit(B):

```text
   ┌────────────┐     parse(A)      ┌──────────┐     emit(B)      ┌────────────┐
   │ harness A  │ ────────────────▶ │    IR    │ ───────────────▶ │ harness B  │
   │ (on disk)  │                   │ Session  │                  │ (on disk)  │
   └────────────┘                   └──────────┘                  └────────────┘
                                         ▲
                                  one shared shape:
                          Session → Message → Block{…}
```

**21 harnesses parse** into the IR ([harnesses](harnesses.md)); any of them ports to the **12 emit targets**.

Code: the IR in `crates/cv-core/src/ir.rs`, emitters in `harness/<h>/emit.rs`, the registry in
`emit.rs`, the `--strict` verifier in `emit/fidelity.rs` ([architecture](architecture.md)).

## The 12 emit targets

| target | id | writes |
|---|---|---|
| Claude Code | `claude` | threaded JSONL under an encoded project dir |
| Codex | `codex` | dated `rollout-*.jsonl` |
| Grok | `grok` | `summary.json` + `chat_history.jsonl` |
| OpenCode | `opencode` | SQLite `opencode.db`: 2.x rows in a fresh store; an existing 1.x layout is kept |
| OpenClaw | `openclaw` | parent-linked `agents/<id>/sessions/*.jsonl` |
| Gemini CLI | `gemini` | legacy `ConversationRecord` JSON |
| Hermes | `hermes` | SQLite `state.db` (needs the `sqlite` build feature) |
| LM Studio | `lmstudio` | chat-app conversation JSON |
| Cline | `cline` | per-task `api_conversation_history.json` |
| Roo Code | `roo` | Cline-format per-task dir |
| Continue | `continue` | `~/.continue/sessions` JSON |
| Qwen Code | `qwen` | 0.25 session JSONL at `~/.qwen/projects/<cwd>/chats/<id>.jsonl` (`--out` takes a Qwen home or its `projects/`); needs a cwd |

The other nine (Cursor, Goose, Zed, Devin CLI, Kimi Code, Kimi CLI, OpenSession, the ChatGPT and
Claude.ai exports) are parse-only; emitting to one exits 2 ("not a supported target").

Emitters write only recorded facts: Codex the source's `cli_version` (or empty), OpenClaw no
`totalTokens`, LM Studio no unknown `tokenCount`. Continue refuses a tool turn without a result.
Gemini message ids derive from session id and turn index, so a re-emit keeps them.

Emitters stage related files, publish last and roll back on failure; they are not crash-atomic: a
crash can leave a published prefix, and a failed rollback is reported. An unreadable OpenClaw
`sessions.json` or unwritable Gemini project registry is an error. Hermes and OpenCode refuse an
unreadable source before creating the destination store.

## `cv port` — one verb for both

`cv port` converts a session's format, rehomes it to a new directory, or both. (`cv convert` exits
with a pointer here.)

```sh
# Convert a Codex session into Claude Code's format (same working directory).
cv port <id> --harness claude

# The source harness rides on the id when a bare prefix would be ambiguous.
cv port codex:019e75e0 --harness opencode

# Dry run: write under a scratch dir instead of the target's real storage root.
cv port <id> --harness gemini --out /tmp/try-gemini

# Rehome a session to a new working directory (same harness).
cv port <id> --cwd ~/work/new-checkout

# Rehome *and* change harness in one step.
cv port <id> --harness claude --cwd ~/work/new-checkout
```

`--harness` names the target (default: the source's, a pure rehome); `--cwd` the new working
directory.

`port` writes into the target's real storage root and prints a resume hint:

```text
✦ wrote /Users/you/.claude/projects/-Users-you-proj/9f3c….jsonl (9f3c…)
  ↳ cd /Users/you/proj && claude --resume 9f3c…
```

Without an installed target, cv asks for `--out <dir>`. The source is never touched.

Rehoming rewrites the cwd the format bakes in (Claude's project dir, Grok's encoded path,
Cline/Roo's `<environment_details>` line).

**Same-harness fidelity.** A same-harness port reads format-complete (`Demand::COMPLETE`) and
replays every record verbatim, meta lines included (Claude's `mode`, `queue-operation`, `ai-title`,
compact boundaries), rewriting only `sessionId` and `cwd`. Uninterpreted records ride as [`carrier`](architecture.md#the-unified-ir)
messages; replay fields live in the harness's own bag (`extra["claude"]`, …) plus the verbatim record
at `_record`. Cross-harness ports use the ordinary full parse.

**Project memory comes along.** Unless `--no-context`, `port` copies the source cwd's context files
into the new directory (`CONTEXT_FILES` in `crates/cv/src/cmd/port.rs`):

```text
CLAUDE.md   CLAUDE.local.md   AGENTS.md   GEMINI.md
MEMORY.md   .cursorrules      .windsurfrules
```

It never overwrites a file and never fails the port.

## What survives, and what's lossy

### Verified emits

Every `cv port` re-parses its output with the target's adapter and diffs it against the source IR
(`emit_verified()`).

Per message it compares the `role` and [`kind`](architecture.md#the-unified-ir) sequences, block-`type`
counts, thinking (text, signature-only, encrypted), tool names on results, `is_error`, and whether
`details`, `usage`, `model`, timestamps and ids survived; per session, title, cwd, model,
`system_prompt` and `lineage`.

Each difference is a delta, reported under `⚠ lossy:`:

- **expected**: the format has no place for it (vendor-bound thinking signatures, per-message ids).
- **unexpected**: the format could have held it; `cv port --strict` exits non-zero.

### Known faithful-but-lossy cases

| target | what's lossy | why |
|---|---|---|
| **LM Studio** | tool calls and results flatten to `[tool call: …]` / `[tool result: …]` text | no tool structures on disk |
| **LM Studio** | thinking becomes a `style.type=="thinking"` text block; no `cwd` is written | chat app: `Session::cwd` is `None`; the cwd appears only in the resume hint |
| **Continue** | thinking flattens into a text part | `ChatMessage` has no reasoning part |
| **Cline** / **Roo** | cwd and task title are embedded in the first user message (`<task>…</task>` + `<environment_details># Current Working Directory (/abs/path) Files`) | the native carrier; the parser reads them back, plus a `task_metadata.json` cwd hint |
| **Grok** | message content collapses to text (thinking in `reasoning`, tools in `tool_calls[]`) | `chat_history` carries text only |
| targets without a system turn (e.g. Claude) | a `Role::System` turn may be dropped | Claude's emitter re-links `parentUuid` over it; a same-harness port replays it verbatim |
| reasoning stored only encrypted | stays encrypted/summary | the plain text was never stored |

## The IR block types

A `Session` is metadata (`id`, `cwd`, `title`, `model`, `git`, `system_prompt`, `lineage`,
timestamps, …) plus `Message`s. Each `Message` has `role` (who), `kind` (what the turn is), `origin`
(where it came from) and `content: Vec<Block>`, tagged by `type` (`Block` in `ir.rs`):

| `type` | carries | notes |
|---|---|---|
| `text` | `text` | |
| `thinking` | `text`, `signature?`, `encrypted?`, `redacted` | `signature` is Anthropic-bound, `encrypted` an opaque OpenAI-bound blob |
| `tool_use` | `id`, `name`, `input` (JSON), `namespace?` | |
| `tool_result` | `tool_use_id`, `content`, `is_error`, `tool_name?`, `status?`, `details?` | |
| `file` | `mime?`, `path?`, `source?` | a file, dir or resource attachment |
| `image` | `media_type?`, `data_ref?` | a path or opaque reference; bytes are not inlined |

Emitters read `kind` and block `type`, never another harness's private keys; the full vocabulary is
in [Architecture](architecture.md#the-unified-ir).

## The OpenSession standard

**OpenSession** is a harness-neutral interchange format proposed to harness authors; cv's
[IR](architecture.md#the-unified-ir) is its reference implementation. Full spec:
**[docs/OPENSESSION.md](https://github.com/emberian/cv/blob/main/docs/OPENSESSION.md)**.

### The shape

```jsonc
{
  "open_session": "0.3",
  "id": "…",
  "harness": "claude",            // origin hint, not identity
  "title": "…",
  "cwd": "/Users/you/project",    // metadata, NOT identity (see below)
  "model": "claude-opus-4-…",     // session default; a message names one only when it differs
  "system_prompt": "…",           // what the harness actually sent, when it stores it
  "lineage": { "forked_from": "…", "parent": "…", "continued_in": "…" },
  "messages": [
    {
      "role": "user",            // WHO spoke: user · assistant · system · tool
      "kind": "prompt",          // WHAT the turn is: prompt · reply · tool_result ·
                                 //   injected_context · notice · compaction_boundary · error · …
      "origin": "human",         // WHERE it came from: human · model · harness · hook ·
                                 //   scheduler · subagent · import
      "timestamp": "2026-…Z",
      "content": [               // an ordered list of typed blocks
        { "type": "text", "text": "…" },
        { "type": "thinking", "text": "…", "signature": "…", "redacted": false },
        { "type": "tool_use", "id": "…", "name": "run_shell", "input": { }, "namespace": "…" },
        { "type": "tool_result", "tool_use_id": "…", "content": "…", "is_error": false },
        { "type": "file", "mime": "…", "path": "…" },
        { "type": "image", "media_type": "image/png", "data_ref": "…" }
      ],
      "extra": { "claude": { } } // namespaced passthrough: one bag per harness
    }
  ]
}
```

0.3 is the IR: `cv export --format json` writes a valid 0.3 document, and
[`cv schema --json`](cli.md#cv-schema) publishes the same shape. cv reads 0.3 only; a 0.2 document is
refused with a pointer to `cv export --format json`. A message without `kind` or `origin` is refused.

### The one heresy: *cwd is metadata, not identity*

OpenSession records `cwd` as a plain field: a session is its messages, portable and resumable
anywhere, unlike harnesses that key it to a directory.

### Why you'd care

A harness that emits OpenSession makes its sessions portable by construction.
