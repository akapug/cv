#!/usr/bin/env bash
# Generate the Claude Code corpus for one installed version: real transcripts Claude Code wrote
# itself, in a throwaway config dir, driven by a cheap real model (Z.ai GLM on its
# Anthropic-compatible endpoint, through ../key_proxy.py, which alone holds the key).
#
#   tools/harness-corpus/claude/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/claude/<version>; it must not exist yet. Needs:
# install.sh <version> run first, python3, git, uuidgen, ~/.zai-key. Env: ZAI_KEY_FILE (default
# ~/.zai-key), CORPUS_MODEL (default glm-5.3-flash), CORPUS_MODEL_ALT (the model-switch target,
# default glm-5.3), MAX_CALLS (default 100; a full run makes about 85).
#
# Every scenario is listed in AFFORDANCES.md with the record it is meant to leave.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools image plan tasks agents background schedule mcp skill workflow worktree web structured errors maxturns interrupt"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/claude/$V/node_modules/.bin/claude"
[ -x "$BIN" ] || { echo "claude $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"; ALT="${CORPUS_MODEL_ALT:-glm-5.3}"; MAX_CALLS="${MAX_CALLS:-100}"
UPSTREAM="https://api.z.ai/api/anthropic"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" CLAUDE_CONFIG_DIR="$CORPUS_HOME/.claude" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR" "$CLAUDE_CONFIG_DIR"
corpus_guard HOME CLAUDE_CONFIG_DIR TMPDIR
PROJECT="$CORPUS_HOME/project"

# The scratch project: a git repo with a few small files, a CLAUDE.md, a notebook, a saved workflow.
cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
printf '# Project notes\n\nThis is a corpus test repository. Keep answers short.\n' >CLAUDE.md
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
cat >nb.ipynb <<'NB'
{"cells": [{"cell_type": "markdown", "id": "intro", "metadata": {}, "source": ["# corpus"]}],
 "metadata": {"kernelspec": {"name": "python3", "display_name": "Python 3", "language": "python"}},
 "nbformat": 4, "nbformat_minor": 5}
NB
mkdir -p .claude/workflows
cat >.claude/workflows/corpus-hello.js <<'JS'
export const meta = {
  name: 'corpus-hello',
  description: 'Two agents each reply with one word',
  phases: [{ title: 'Say' }],
}
const words = await parallel(['ok', 'yes'].map(w => () =>
  agent(`Reply with the single word ${w}. Do not use any tools.`, { label: `say:${w}`, phase: 'Say' })))
return { words }
JS
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# User-level config in the throwaway dir: hooks (each leaves an attachment record), a skill, an agent.
cat >"$CLAUDE_CONFIG_DIR/settings.json" <<'JSON'
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "echo corpus session start hook"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo '{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"corpus hook: the project is a test fixture\"}}'"}]}],
    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "true"}]}],
    "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "echo corpus post-edit hook"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "true"}]}]
  }
}
JSON
mkdir -p "$CLAUDE_CONFIG_DIR/skills/corpus-greet" "$CLAUDE_CONFIG_DIR/agents"
cat >"$CLAUDE_CONFIG_DIR/skills/corpus-greet/SKILL.md" <<'MD'
---
name: corpus-greet
description: Greet the user in exactly three words. Use when asked for a corpus greeting.
---
Reply with exactly three words of greeting and nothing else. Do not use any tools.
MD
cat >"$CLAUDE_CONFIG_DIR/agents/echoer.md" <<'MD'
---
name: echoer
description: Replies with the word it is given. Use when asked to echo a word through a sub-agent.
tools: Read
---
Reply with exactly the word you were asked to echo, nothing else.
MD
cat >"$WORK/mcp.json" <<JSON
{"mcpServers": {"corpus": {"type": "stdio", "command": "python3", "args": ["-I", "$HERE/../codex/mcp_server.py"]}}}
JSON

# Only the proxy sees the real key; Claude Code gets a dummy token for 127.0.0.1.
corpus_proxy "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}" passthrough "$UPSTREAM"

# Claude Code keeps background-task output and pasted images under $CLAUDE_CODE_TMPDIR (default
# /tmp/claude-<uid>/, shared with every real session on the machine), so that goes in $WORK too.
# A clean environment: nothing from the calling shell (a parent Claude Code session exports its own
# CLAUDE_CODE_* and messaging-socket variables) reaches the harness.
CENV=(PATH=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin HOME="$HOME" CLAUDE_CONFIG_DIR="$CLAUDE_CONFIG_DIR"
  TMPDIR="$TMPDIR" CLAUDE_CODE_TMPDIR="$TMPDIR" TERM=dumb LANG=en_US.UTF-8 TZ=UTC
  ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" ANTHROPIC_AUTH_TOKEN=corpus-local-proxy
  ANTHROPIC_MODEL="$MODEL" ANTHROPIC_SMALL_FAST_MODEL="$MODEL" ANTHROPIC_DEFAULT_HAIKU_MODEL="$MODEL"
  ANTHROPIC_DEFAULT_SONNET_MODEL="$MODEL" ANTHROPIC_DEFAULT_OPUS_MODEL="$MODEL" CLAUDE_CODE_SUBAGENT_MODEL="$MODEL"
  DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1
  CLAUDE_CODE_MAX_RETRIES=1 ENABLE_TOOL_SEARCH=true CLAUDE_CODE_ENABLE_TODO_TOOLS=1)

# cc [VAR=val ...] -- <claude args> <prompt>: one headless `claude -p` run in the project. The prompt
# goes after `--` because --tools and --allowedTools are variadic and would swallow it.
cc() {
  local extra=()
  while [ "$1" != "--" ]; do extra+=("$1"); shift; done; shift
  local prompt="${!#}"
  echo "--- claude $*" | cut -c1-150
  (cd "$PROJECT" && env -i "${CENV[@]}" ${extra[@]+"${extra[@]}"} "$BIN" -p --output-format json --permission-mode dontAsk \
    "${@:1:$#-1}" -- "$prompt" </dev/null >>"$WORK/run.log" 2>&1) || echo "    (exit $?)"
}
newid() { uuidgen | tr 'A-Z' 'a-z'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()
# Small tool sets keep each session's request ledger small: since 2.1.300 every API call leaves an
# `api-request-shape` (the system prompt + every tool schema, ~6-40 KB, one per distinct shape) and
# content-addressed `api-request-blob`s. The older prompt_snapshot attachment records the same prompt
# once more; it is kept in the chat session only and switched off everywhere else.
NOSKILLS=(--disable-slash-commands --system-prompt-snapshot off)
SNAP=(--disable-slash-commands)

if want chat; then   # multi-turn, resume, a named session, a model switch, /compact, a forked resume
  id="$(newid)"
  cc -- --session-id "$id" -n "corpus chat" --tools "" "${SNAP[@]}" "Say hello in five words."
  cc -- --resume "$id" --tools "" "${SNAP[@]}" "Now say it in French."
  cc -- --resume "$id" --tools "" --model "$ALT" "${SNAP[@]}" "Which language did you use first? One word."
  cc -- --resume "$id" --tools "" "/compact"
  cc -- --resume "$id" --tools "" "${SNAP[@]}" "What did we talk about? One short sentence."
  cc -- --resume "$id" --fork-session --tools "" "${SNAP[@]}" "Say goodbye in three words."
  SCEN+=("chat" "resume" "session_name" "model_switch" "compaction" "fork_session" "thinking" "hooks" "claude_md")
fi
if want tools; then  # read/edit/write/shell, parallel calls, an error, a denial, an oversized result, a notebook
  id="$(newid)"
  T=(--tools "Read,Edit,Write,Bash,NotebookEdit" "${NOSKILLS[@]}")
  # File checkpointing (file-history-snapshot records, file-history/ backups) is off in -p unless asked
  # for; a small Bash output cap makes `seq 1 1000` spill to tool-results/<id>.txt as a long one would.
  E=(CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=1 BASH_MAX_OUTPUT_LENGTH=2000)
  A=(--allowedTools "Read Edit Write NotebookEdit Bash(cat *) Bash(seq *)")
  cc "${E[@]}" -- --session-id "$id" "${T[@]}" "${A[@]}" "Read a.txt and b.txt at the same time (two parallel Read calls), then reply with their combined line count."
  cc "${E[@]}" -- --resume "$id" "${T[@]}" "${A[@]}" "Use the Edit tool to replace TODO with DONE in notes.txt, then run with Bash: cat notes.txt"
  cc "${E[@]}" -- --resume "$id" "${T[@]}" "${A[@]}" "Run with Bash: cat missing-file.txt   then tell me the error in one line."
  cc "${E[@]}" -- --resume "$id" "${T[@]}" "${A[@]}" "Run with Bash: rm b.txt   (it may be refused; if so, just say refused)."
  cc "${E[@]}" -- --resume "$id" "${T[@]}" "${A[@]}" "Run exactly this Bash command, unmodified and without pipes: seq 1 1000   then tell me only the last number."
  cc "${E[@]}" -- --resume "$id" "${T[@]}" "${A[@]}" "Use the Write tool to create c.txt containing the word delta, then use NotebookEdit to insert a code cell with print(1) after the cell with id intro in nb.ipynb."
  SCEN+=("tools" "parallel_tools" "tool_error" "permission_denied" "large_tool_result" "write" "notebook" "file_history")
fi
if want image; then  # a user image (stream-json input) and an image read by a tool
  id="$(newid)"
  B64="$(base64 <"$PROJECT/dot.png" | tr -d '\n')"
  printf '%s\n' "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"image\",\"source\":{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"$B64\"}},{\"type\":\"text\",\"text\":\"An image is attached. Do not use tools. Reply with the single word: received.\"}]}}" >"$WORK/image.jsonl"
  echo "--- claude (stream-json image input)"
  (cd "$PROJECT" && env -i "${CENV[@]}" "$BIN" -p --input-format stream-json --output-format stream-json --verbose \
    --permission-mode dontAsk --session-id "$id" --tools "Read" "${NOSKILLS[@]}" <"$WORK/image.jsonl" >>"$WORK/run.log" 2>&1) || echo "    (exit $?)"
  cc -- --resume "$id" --tools "Read" "${NOSKILLS[@]}" "Use the Read tool on dot.png and say what size it is."
  SCEN+=("image_input" "image_tool_result")
fi
if want plan; then   # plan mode: the permission mode recorded on the turn, and the plan-mode attachments
  cc -- --permission-mode plan --tools "Read" "${NOSKILLS[@]}" "Without reading or changing any file, give a two-step plan for renaming b.txt to c.txt."
  SCEN+=("plan_mode")
fi
if want tasks; then  # the task list (TaskCreate/TaskUpdate/TaskList), persisted under tasks/
  cc -- --tools "TaskCreate,TaskUpdate,TaskList,TaskGet" --allowedTools "TaskCreate TaskUpdate TaskList TaskGet" "${NOSKILLS[@]}" \
    "Create two tasks with TaskCreate (subjects: alpha, beta). Then mark alpha completed with TaskUpdate. Then call TaskList and report it in one line."
  SCEN+=("task_list")
fi
if want agents; then # Agent tool: a foreground sub-agent, a custom agent type, a background agent and its notification
  id="$(newid)"
  cc -- --session-id "$id" --tools "Agent,Read,SendMessage" --allowedTools "Agent Read SendMessage" "${NOSKILLS[@]}" \
    "Use the Agent tool with subagent_type echoer to echo the word ok, then use the Agent tool with subagent_type general-purpose and the prompt 'Read a.txt and reply with its first line' and report both answers."
  cc -- --resume "$id" --tools "Agent,Read,SendMessage" --allowedTools "Agent Read SendMessage" "${NOSKILLS[@]}" \
    "Use SendMessage to the echoer agent you started (by its agent id) asking it to echo the word again, and report its reply."
  # Asked as a fork (`subagent_type: fork`, named: a model that omits the type gets a general-purpose
  # child with `launchedWithDefaults`). Since 2.1.302 fork sub-agents are on by default in `-p`: the
  # child's transcript opens with a `fork-context-ref` (parent session + the uuid its context was cut
  # at) and its meta.json says `agentType: fork, isFork: true`. (2.1.296 spawned a plain general-purpose
  # child even with CLAUDE_CODE_FORK_SUBAGENT=1.)
  cc CLAUDE_CODE_FORK_SUBAGENT=1 -- --tools "Agent" --allowedTools "Agent" "${NOSKILLS[@]}" \
    "Use the Agent tool with subagent_type set to exactly fork (a fork of yourself) and the prompt 'Reply with the single word forked.' and report its reply."
  cc -- --tools "Agent,Read" --allowedTools "Agent Read" "${NOSKILLS[@]}" \
    "Use the Agent tool with run_in_background set to true, subagent_type echoer, to echo the word later. Then wait for its completion notification and report the word."
  SCEN+=("subagent" "custom_agent_type" "send_message" "fork_subagent" "background_agent" "agent_notification")
fi
if want background; then # background Bash and Monitor (a deferred tool, loaded through ToolSearch)
  # Monitor is not reachable here: it is gated on a remote feature flag (tengu_amber_sentinel) that a
  # third-party endpoint never receives. (With ToolSearch also listed, 2.1.296 sent the model no Bash.)
  cc -- --tools "Bash" --allowedTools "Bash(sleep *) Bash(echo *)" "${NOSKILLS[@]}" \
    "Run this command with the Bash tool with run_in_background set to true: sleep 3; echo corpus-done   Then end your turn without any other tool call. When its task-notification arrives, report the output in one line."
  SCEN+=("background_bash" "task_notification")
  SKIP+=("monitor (remote feature flag tengu_amber_sentinel; never on for a third-party endpoint)")
fi
if want schedule; then # timers: CronCreate/CronList/CronDelete and ScheduleWakeup (none fire in -p)
  cc -- --tools "CronCreate,CronList,CronDelete,ScheduleWakeup" --allowedTools "CronCreate CronList CronDelete ScheduleWakeup" "${NOSKILLS[@]}" \
    "Use CronCreate to schedule the prompt 'say tick' with cron expression '*/30 * * * *'. Then CronList. Then CronDelete that job. Then call ScheduleWakeup with delaySeconds 60 and the prompt 'say awake'. Then report done."
  SCEN+=("cron" "schedule_wakeup")
fi
if want mcp; then    # an MCP tool call through the stdio server next to the Codex generator; with ToolSearch
  # on, MCP tools are deferred, so the model loads it first (tool_reference blocks)
  cc -- --mcp-config "$WORK/mcp.json" --strict-mcp-config --tools "ToolSearch" --allowedTools "mcp__corpus__add ToolSearch" "${NOSKILLS[@]}" \
    "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp" "tool_search_deferred")
fi
if want skill; then  # a user skill invoked as a slash command
  id="$(newid)"
  cc -- --session-id "$id" --tools "Skill" --system-prompt-snapshot off "/corpus-greet"
  cc -- --resume "$id" --tools "Skill" --allowedTools "Skill" --system-prompt-snapshot off "Use the Skill tool to run the corpus-greet skill again."
  SCEN+=("skill_slash_command" "skill_tool")
fi
if want workflow; then # a saved workflow run by the Workflow tool: workflows/<run>.json, subagents/workflows/<run>/
  cc -- --tools "Workflow" --allowedTools "Workflow" "${NOSKILLS[@]}" \
    "Run the saved workflow named corpus-hello with the Workflow tool (set only its name input). Wait for its completion notification, then report the words it returned."
  SCEN+=("workflow")
fi
if want worktree; then # EnterWorktree/ExitWorktree: a git worktree under .claude/worktrees, and the session's cwd moving
  cc -- --tools "EnterWorktree,ExitWorktree" --allowedTools "EnterWorktree ExitWorktree" "${NOSKILLS[@]}" \
    "Use EnterWorktree to create a worktree named corpus-wt. Then use ExitWorktree with action keep. Then say done."
  SCEN+=("worktree")
fi
if want web; then    # WebFetch (fetches, then a small-model call summarizes)
  cc -- --tools "WebFetch" --allowedTools "WebFetch(domain:example.com)" "${NOSKILLS[@]}" \
    "Use WebFetch on https://example.com and tell me the page title."
  SCEN+=("web_fetch")
fi
if want structured; then # --json-schema: the StructuredOutput tool
  cc -- --tools "" --json-schema '{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"]}' "${NOSKILLS[@]}" \
    "What is 6 times 7? Answer in the structured output."
  SCEN+=("structured_output")
fi
if want errors; then # a provider failure: the proxy answers CORPUS-PROVIDER-ERROR with a 500 (never forwarded), so
  # Claude Code retries once (CLAUDE_CODE_MAX_RETRIES=1: a `system/api_error` retry record) and gives up (a
  # synthetic `assistant` API-error turn); then the same session resumes normally.
  id="$(newid)"
  cc -- --session-id "$id" --tools "" "${NOSKILLS[@]}" "CORPUS-PROVIDER-ERROR: reply with the single word ok."
  cc -- --resume "$id" --tools "" "${NOSKILLS[@]}" "Reply with the single word recovered."
  SCEN+=("provider_error" "api_error_retry")
fi
if want maxturns; then # --max-turns: the run stops after one model turn, leaving a max_turns_reached attachment
  cc -- --max-turns 1 --tools "Read" --allowedTools "Read" "${NOSKILLS[@]}" \
    "Read a.txt with the Read tool, then read b.txt with the Read tool, then report both first lines."
  SCEN+=("max_turns")
fi
if want interrupt; then # an interrupted turn: SIGINT while a tool is running
  id="$(newid)"
  echo "--- claude (interrupted)"
  (cd "$PROJECT" && exec env -i "${CENV[@]}" "$BIN" -p --output-format json --permission-mode dontAsk --session-id "$id" \
    --tools "Bash" --allowedTools "Bash(sleep *)" "${NOSKILLS[@]}" "Run with Bash: sleep 40   and then say done." \
    </dev/null >>"$WORK/run.log" 2>&1) &
  ipid=$!
  # Wait for the model's Bash tool_use (not the tool's schema: since 2.1.300 the request ledger
  # writes every tool name into the transcript before the model has answered).
  for _ in $(seq 60); do grep -Eq '"type":"tool_use","id":"[^"]*","name":"Bash"' "$CLAUDE_CONFIG_DIR"/projects/*/"$id".jsonl 2>/dev/null && break; sleep 0.5; done
  sleep 1; kill -INT "$ipid" 2>/dev/null || true; wait "$ipid" 2>/dev/null || true
  SCEN+=("interrupt")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"

# Copy what Claude Code wrote about the sessions: projects/ (transcripts, sub-agent transcripts,
# workflow run records, tool-result spill files), tasks/ and file-history/. Not settings, backups, .claude.json,
# shell snapshots, plugins, caches or the session-env dir.
mkdir -p "$OUT/.claude"
cp -R "$CLAUDE_CONFIG_DIR/projects" "$OUT/.claude/"
for d in tasks todos file-history; do [ -d "$CLAUDE_CONFIG_DIR/$d" ] && cp -R "$CLAUDE_CONFIG_DIR/$d" "$OUT/.claude/"; done
[ -e "$CLAUDE_CONFIG_DIR/history.jsonl" ] && cp "$CLAUDE_CONFIG_DIR/history.jsonl" "$OUT/.claude/"
# The memory dir Claude Code creates per project is empty; an empty dir is not worth committing.
find "$OUT" -type d -empty -delete
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"claude\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL (switch target $ALT) via Z.ai's Anthropic endpoint, through key_proxy.py\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$( [ ${#SKIP[@]} -gt 0 ] && printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
