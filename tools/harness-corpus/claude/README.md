# Claude Code corpus generator

```sh
tools/harness-corpus/claude/install.sh 2.1.296
TMPDIR=<your scratch dir> tools/harness-corpus/claude/generate.sh 2.1.296 crates/cv-core/tests/corpus/claude/2.1.296
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, uuidgen, and `~/.zai-key` (override with `ZAI_KEY_FILE`).

`install.sh` installs `@anthropic-ai/claude-code@<v>` into `~/tools/harness-versions/claude/<v>/`
with `npm --prefix` (the package pulls the platform's native binary as an optional dependency; for
2.1.296 it is byte-identical to the one in `~/.local/share/claude/versions/`). The installed
`claude`, `~/.claude` and `~/.local/share/claude` are never read or written.

## Isolation

Every run is `env -i` with `HOME`, `CLAUDE_CONFIG_DIR`, `TMPDIR` and `CLAUDE_CODE_TMPDIR` inside the
throwaway dir. `env -i` matters: a parent Claude Code session exports `CLAUDE_CODE_*` variables,
including a messaging socket, that would otherwise reach the child. `CLAUDE_CODE_TMPDIR` matters too:
Claude Code writes background-task output and pasted images to `/tmp/claude-<uid>/` by default,
which every real session on the machine shares. With `CLAUDE_CONFIG_DIR` set, the keychain entry
name is suffixed, so ember's login is never found.

## The model path

Claude Code talks to `http://127.0.0.1:<free port>` with a dummy token. `../key_proxy.py` holds the
Z.ai key, forwards to `https://api.z.ai/api/anthropic` (Anthropic Messages, streaming), counts calls
into `calls.jsonl` (shape only) and refuses past `MAX_CALLS` (default 80). Every model alias is
pointed at `glm-5.3-flash`; one resume switches to `glm-5.3` to record a model change. The records
are Claude Code's own; what differs from a first-party run is the model name, Z.ai's short
24-hex thinking `signature` instead of Anthropic's long one, and the "unrecognized model" notice on
stderr.

## Scenarios

`AFFORDANCES.md` lists every affordance, the record it leaves, and whether the corpus has it.
`generate.sh --only "<names>"` runs a subset: `chat tools image plan tasks agents background
schedule mcp skill workflow worktree web structured errors maxturns interrupt`. A full run made 80
model calls in about 9 minutes (the `ScheduleWakeup` scenario waits for its wakeup to fire). The
`errors` scenario never reaches the upstream: the proxy answers its CORPUS-PROVIDER-ERROR prompt
with a 500, which is what makes Claude Code write a retry record and a synthetic API-error turn.

Kept small on purpose: each `-p` run gets `--tools` limited to what its scenario needs, and
`--system-prompt-snapshot off` everywhere but the chat session. Since 2.1.300 the size driver is the
request ledger Claude Code writes for every API call (`api-request`, `api-request-shape` with the
system prompt and every tool schema, `api-request-blob` per message), which no flag turns off; it is
the format, so the corpus keeps it and `lib.sh`'s hard cap is 1.5 MB.

If every run exits 1 with `API Error: 502 key_proxy upstream: URLError`, the proxy's Python cannot
verify the upstream's TLS certificate (a python.org build without certificates): run the generator
with `SSL_CERT_FILE=/etc/ssl/cert.pem`.

## What is copied

`projects/` (transcripts, `subagents/`, `workflows/`, `tool-results/`), `tasks/`, `file-history/`.
The encoded-cwd dir name carries the throwaway path; `scrub.py` renames it to `-home-corpus-project`
and rewrites the dash-encoded form inside files. Not copied: `settings.json`, `.claude.json` and its
backups, `session-env/`, `shell-snapshots/`, the skills/agents the generator wrote.
