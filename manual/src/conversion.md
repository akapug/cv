# Cross-harness conversion

Every agent harness stores its sessions a little differently — Claude Code threads JSONL
by `parentUuid`, Codex writes dated `rollout-*.jsonl` files, OpenCode keeps messages in SQLite (older stores shard messages and
parts across directories), Cline buries the working directory inside the first user message.
clustervision's trick is that it never converts harness-A-format *directly* into
harness-B-format. Instead, **every harness parses _into_ one unified IR**, and conversion
is just:

```text
   ┌────────────┐     parse(A)      ┌──────────┐     emit(B)      ┌────────────┐
   │ harness A  │ ────────────────▶ │    IR    │ ───────────────▶ │ harness B  │
   │ (on disk)  │                   │ Session  │                  │ (on disk)  │
   └────────────┘                   └──────────┘                  └────────────┘
                                         ▲
                                  one shared shape:
                          Session → Message → Block{…}
```

Because the IR sits in the middle, you don't need an N×N matrix of converters — you need
N parsers and a handful of emitters. **21 harnesses can be parsed** _into_ the IR
([harnesses](harnesses.md)); of those, **12 can also be conversion _targets_** that the IR
can be emitted back _out_ to. Any emit-capable target can receive a session that originated
in *any* of the 21, so the practical conversion space is "21 sources × 12 targets". 🔮

The IR itself is defined in `crates/cv-core/src/ir.rs`. Each emitter lives beside its reader
(`crates/cv-core/src/harness/<h>/emit.rs`); `crates/cv-core/src/emit.rs` holds the registry and
`emit/fidelity.rs` the `--strict` verifier. For the broader picture of how parsers and emitters fit
together, see [architecture](architecture.md).

## The 12 emit targets

These are the harnesses `emit()` can currently write — the authoritative list is
`supported_targets()` in `emit.rs`:

| target | id | notes |
|---|---|---|
| Claude Code | `claude` | threaded JSONL under an encoded project dir |
| Codex | `codex` | dated `rollout-*.jsonl` |
| Grok | `grok` | `summary.json` + `chat_history.jsonl` |
| OpenCode | `opencode` | SQLite `opencode.db`: fresh stores use 2.x rows; existing 1.x database layouts are preserved |
| OpenClaw | `openclaw` | parent-linked `agents/<id>/sessions/*.jsonl` |
| Gemini CLI | `gemini` | legacy `ConversationRecord` JSON |
| Hermes | `hermes` | SQLite `state.db` *(requires the `sqlite` build feature)* |
| LM Studio | `lmstudio` | chat-app conversation JSON |
| Cline | `cline` | per-task `api_conversation_history.json` |
| Roo Code | `roo` | Cline-format per-task dir |
| Continue | `continue` | `~/.continue/sessions` JSON |
| Qwen Code | `qwen` | 0.25 session JSONL, `~/.qwen/projects/<cwd>/chats/<id>.jsonl` (`--out` takes a Qwen home or its `projects/`); needs a cwd |

> The other nine parseable harnesses — **Cursor**, **Goose**, **Zed**, **Devin CLI**, **Kimi Code**, **OpenSession**, **Kimi CLI** (its
> `~/.kimi` store is frozen; kimi-cli migrates it to `~/.kimi-code` on open), and the
> **ChatGPT/Claude.ai account data exports** — are parse-only. You can read, search, and port *away* from them, but
> they aren't port targets yet (their on-disk stores are harder to write back
> safely). Asking to emit to one is a usage error ("not a supported target", exit 2) rather than
> corrupting anything.

## `cv port` — one verb for both

Converting a session's format and rehoming it to a new directory were always the same
act: **produce a copy of this session that runs elsewhere.** Since 0.11.0 they are one
command. (`cv convert` is gone; it errors with a pointer to this.)

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

`--harness` names the target harness; omitted, it defaults to the source's, which makes
a pure rehome. `--cwd` names the new working directory. Neither is spelled `--to`
any more: a flag says what its value *is*, not where it goes.

By default `port` writes into the target harness's real storage root (so the ported
session shows up when you launch that harness), and prints a resume hint:

```text
✦ wrote /Users/you/.claude/projects/-Users-you-proj/9f3c….jsonl (9f3c…)
  ↳ claude --resume 9f3c…  (run from /Users/you/proj)
```

If the target harness doesn't appear to be installed, `cv` asks you to pass `--out <dir>`
rather than guessing where its store lives. The source session is **never** touched.

Rehoming rewrites the working directory baked into the target format (Claude's encoded
project-dir name, Grok's percent-encoded path, Cline/Roo's `<environment_details>` cwd
line, …) so the ported session resolves to the new location.

**Same-harness fidelity.** When the source and target harness are the same (a pure rehome,
or an A→A port), the session is parsed **format-complete** (`ParseOptions::complete()`):
every record — including the non-conversational meta lines the lean passes skip (Claude's
`mode`/`queue-operation`/`ai-title`/compact-boundary records, exhaustive per-record
`extra` fields) — is carried through the IR and replayed verbatim into the output, with only
the session-identity fields (`sessionId`, `cwd`) rewritten to the new home. Records the
adapter doesn't interpret ride along as [`carrier`](architecture.md#the-unified-ir) messages,
and the replay fields live in the harness's own bag (`extra["claude"]`, …) plus the verbatim
record at `_record` — so an emitter only ever reads back its *own* harness's keys. Cross-harness
porting uses the ordinary full-fidelity parse, since one harness's raw records can't be
replayed into another's format.

**It also brings the project's memory along.** Unless you pass `--no-context`, `port`
copies the source cwd's context files into the new directory so the ported session lands
with its instructions/memory intact. The carried set (`CONTEXT_FILES` in `main.rs`) is:

```text
CLAUDE.md   CLAUDE.local.md   AGENTS.md   GEMINI.md
MEMORY.md   .cursorrules      .windsurfrules
```

This copy is strictly best-effort: it **never overwrites** an existing file at the target
(it tells you it left it as-is) and never fails the port if a copy doesn't work.

## What survives, and what's lossy

The IR is deliberately a *superset* — fields a given harness lacks are simply `None`/empty,
and harness-specific extras ride along in `Message::extra`. So a clean round-trip is the
common case. But some target formats genuinely cannot represent some IR content, and
clustervision tells you when that happens instead of pretending otherwise.

### Verified emits

Every `cv port` runs `emit_verified()`, which does the honest thing: after writing the
output, it **re-parses that output with the target's own adapter** and diffs it against the
source IR, message by message. This is purely a read-back check — it never changes what
`emit()` wrote.

What it compares, per message: the `role` sequence, the [`kind`](architecture.md#the-unified-ir)
sequence, block-`type` counts, thinking (distinguishing text, signature-only and encrypted),
tool names on results, `is_error`, whether `details` survived, and whether `usage`, `model`,
timestamps and ids survived. Plus, per session: title, cwd, model, `system_prompt` and
`lineage`.

Every difference becomes a **delta**, and every delta is classified against a per-emitter
table of what that format genuinely cannot carry:

- **expected** — the target format has no place for this (vendor-bound thinking signatures,
  per-message ids in a format that doesn't store them). Reported under `⚠ lost`, never fatal.
- **unexpected** — the target *could* have held it and didn't. Reported the same way, and
  `cv port --strict` exits non-zero on any of them.

An empty delta list means a fully clean round-trip. The point of the split is that "lossy"
stays *visible* without crying wolf: a format limitation and a bug look different in the
output, and `--strict` fails on exactly one of them.

### Known faithful-but-lossy cases

These are honest format limitations of the *target*, not bugs in the converter — the
emitter does the most faithful thing the destination format allows:

| target | what's lossy | why |
|---|---|---|
| **LM Studio** | tool calls & tool results flatten to text like `[tool call: …]` / `[tool result: …]` | LM Studio is a chat app with **no first-class tool structures** on disk, so we don't fabricate any |
| **LM Studio** | thinking is kept but as a `style.type=="thinking"` text block; no `cwd` is written | it's a chat app — `Session::cwd` is always `None`; the chosen cwd only appears in the resume hint |
| **Continue** | thinking flattens into a plain text part | Continue's `ChatMessage` has no thinking/reasoning part, so reasoning is preserved as searchable text |
| **Cline** / **Roo** | the cwd and task title are **embedded into the first user message text** (`<task>…</task>` + `<environment_details># Current Working Directory (/abs/path) Files`) | that's how Cline/Roo natively carry cwd/title — the parser reads them right back out of the transcript, plus a `task_metadata.json` cwd-hint sidecar |
| **Grok** | per-message content collapses to plain text (thinking rides in a separate `reasoning` field, tools in `tool_calls[]`) | Grok's `chat_history` carries text only |
| any target without a standalone system turn (e.g. Claude) | a `Role::System` turn may be dropped | the format has no place for a standalone system message; dropping it avoids polluting user text (Claude's emitter re-links `parentUuid` threading over the dropped record, and a same-harness port replays the original record verbatim instead of dropping it) |
| reasoning that was only ever an encrypted blob | comes back as encrypted/summary only | the raw chain-of-thought text was never stored to begin with |

When any of these reduce the content on the way out, `emit_verified` surfaces it as a
warning — so "lossy" is always *visible*, never silent.

## The IR block types

A `Session` is metadata (`id`, `cwd`, `title`, `model`, `git`, `system_prompt`, `lineage`,
timestamps, …) plus a list of `Message`s. Each `Message` says **who** speaks (`role`),
**what** the turn is (`kind`) and **where** it came from (`origin`), and carries a
`content: Vec<Block>`. Blocks are tagged by `type` (see `Block` in `ir.rs`):

| `type` | carries | notes |
|---|---|---|
| `text` | `text` | plain assistant/user prose |
| `thinking` | `text`, `signature?`, `encrypted?`, `redacted` | extended reasoning; `signature` is Anthropic-bound, `encrypted` an opaque OpenAI-bound blob |
| `tool_use` | `id`, `name`, `input` (JSON), `namespace?` | a tool/function invocation by the assistant |
| `tool_result` | `tool_use_id`, `content`, `is_error`, `tool_name?`, `status?`, `details?` | the result fed back for a call |
| `file` | `mime?`, `path?`, `source?` | a first-class file/dir/resource attachment |
| `image` | `media_type?`, `data_ref?` | image bytes are **not** inlined into the IR — only a path/opaque reference is kept |

Emitters read `kind` and block `type`, never another harness's private keys — which is what
makes any-to-any porting a translation rather than a pile of special cases. The full
vocabulary (every `kind`, every `origin`, `lineage`, and the `extra["<harness>"]` rule) is
defined once in [Architecture](architecture.md#the-unified-ir). For the interchange shape of
a parsed session, see [the OpenSession standard](#the-opensession-standard) below.

## The OpenSession standard

After reverse-engineering twenty-one different transcript formats, one thing was obvious: they're all
*almost the same thing* underneath. **OpenSession** is the format they should have agreed on — a
small, honest, harness-neutral interchange format, written down as a proposal for harness authors.
clustervision's internal [IR](architecture.md#the-unified-ir) is its reference implementation: the
same spine, the same blocks, the same heresy about `cwd`.

The full spec lives at **[docs/OPENSESSION.md](https://github.com/emberian/cv/blob/main/docs/OPENSESSION.md)**. The essentials:

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

> **0.3 is the IR.** Until 0.11.0 this page showed a camelCase document with `kind`-tagged blocks
> while cv's own output was snake_case with `type`-tagged ones, and the spec claimed the IR was its
> reference implementation anyway — so the two described different formats while asserting they
> were one. 0.3 resolves it in the direction that leaves one vocabulary: the spec adopts the IR.
> `cv export --format json` emits a valid OpenSession 0.3 document, and what
> [`cv schema --json`](cli.md#cv-schema) publishes is the same shape. cv reads 0.3 only: a 0.2
> document is refused with a pointer at `cv export --format json`.

### The one heresy: *cwd is metadata, not identity*

Most harnesses key a session to the exact directory it ran in, so moving a project loses the thread.
OpenSession records `cwd` as a plain field. A session is its **messages** — it can be read, ported,
and resumed anywhere. This is what makes [porting](cli.md) and [cross-harness conversion](conversion.md)
possible at all.

### Why you'd care

If you ship a harness: emit OpenSession and your users' sessions become portable *by construction* —
every other tool can read, search, and convert them. 🤝
