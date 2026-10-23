#!/usr/bin/env bash
# Generate the Codex corpus for one installed version: real rollouts Codex wrote itself, in a
# throwaway home, driven by a cheap real model (Z.ai GLM through ../key_proxy.py --translate responses).
#
#   tools/harness-corpus/codex/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/codex/<version>; it must not exist yet (a
# committed version is never regenerated in place). Needs: install.sh <version> run first, python3,
# git, ~/.zai-key. Env: ZAI_KEY_FILE (default ~/.zai-key), CORPUS_MODEL (default glm-5.3-flash),
# MAX_CALLS (default 40).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0; ONLY="chat tools image compact mcp subagent"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/codex/$V/node_modules/.bin/codex"
[ -x "$BIN" ] || { echo "codex $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"
# The model slug Codex is TOLD, for its bundled metadata (freeform apply_patch, reasoning summaries,
# responses-lite tool prefixes). The proxy answers every request with $MODEL regardless.
PROFILE="${CODEX_MODEL_PROFILE:-gpt-6-luna}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" CODEX_HOME="$CORPUS_HOME/.codex" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME OPENAI_API_KEY OPENAI_BASE_URL
corpus_guard HOME CODEX_HOME TMPDIR
PROJECT="$CORPUS_HOME/project"
# Only the proxy sees the real key; Codex gets a dummy one for its provider at $PORT.
corpus_proxy "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}" responses "$UPSTREAM" --model "$MODEL"
export CORPUS_SHIM_KEY="corpus-local-shim"

# The scratch project: a git repo with a few small files.
cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

cat >"$CODEX_HOME/instructions.md" <<'MD'
You are a coding agent in a small test repository. Be brief. Use tools when asked.
MD
cat >"$CODEX_HOME/config.toml" <<TOML
model = "$PROFILE"
model_provider = "corpus"
model_instructions_file = "$CODEX_HOME/instructions.md"
approval_policy = "never"
sandbox_mode = "workspace-write"
check_for_update_on_startup = false

[analytics]
enabled = false

[model_providers.corpus]
name = "corpus shim (Z.ai GLM)"
base_url = "http://127.0.0.1:$PORT/v1"
wire_api = "responses"
env_key = "CORPUS_SHIM_KEY"
request_max_retries = 0
stream_max_retries = 0

[mcp_servers.corpus]
command = "python3"
args = ["-I", "$HERE/mcp_server.py"]
# Without this, approval_policy = "never" makes Codex refuse the call ("requires approval").
default_tools_approval_mode = "approve"

[projects."$PROJECT"]
trust_level = "trusted"
TOML

HOOK_ARGS=()
run() { echo "--- codex exec $*" | cut -c1-140; "$BIN" exec --skip-git-repo-check "${HOOK_ARGS[@]}" -C "$PROJECT" "$@" </dev/null >>"$WORK/exec.log" 2>&1 || echo "    (exit $?)"; }
last_id() { ls -t "$CODEX_HOME"/sessions/*/*/*/rollout-*.jsonl | head -1 | sed -E 's/.*-([0-9a-f-]{36})\.jsonl$/\1/'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=()

if want chat; then   # 1 plain multi-turn chat + 9 resume (exec resume continues the same thread)
  run "Say hello in five words."
  id="$(last_id)"
  run resume "$id" "Now say it in French."
  run resume "$id" "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error, 5 reasoning (every turn)
  run "Read a.txt and b.txt at the same time (two parallel tool calls), then reply with their combined line count."
  id="$(last_id)"
  run resume "$id" "Use apply_patch to replace TODO with DONE in notes.txt, then run: cat notes.txt"
  run resume "$id" "Run: cat missing-file.txt   and tell me the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error" "reasoning")
fi
if want image; then  # 6 image input (persisted as input_image; the text model gets a placeholder)
  run --image="$PROJECT/dot.png" -- "An image is attached. Do not use any tools. Reply with the single word: received."
  SCEN+=("image")
fi
if want compact; then # 8 compaction: a tiny auto-compact limit forces it on the resumed turn
  run "List three prime numbers."
  id="$(last_id)"
  run -c model_auto_compact_token_limit=200 resume "$id" "List three more."
  SCEN+=("compaction")
fi
if want mcp; then    # 10 MCP tool call through the stdio server next to this script
  run "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want subagent; then # 7 sub-agents (multi_agent feature)
  run --enable multi_agent "Spawn one sub-agent with the task 'reply with the word ok', wait for it, then report what it said."
  SCEN+=("subagent")
fi

if want hooks; then
  # One prior real editor gives start a catalog row and edit a live competing writer.
  CV="${CV_HOOK_BINARY:?hooks scenario needs an absolute CV_HOOK_BINARY}"
  export CLUSTERVISION_HOME="$WORK/cv-home" CV_ENDPOINT="corpus:hooks"
  mkdir -p "$CLUSTERVISION_HOME"
  run "Use tools.apply_patch through functions.exec to apply this exact patch (one line per comma): *** Begin Patch, *** Update File: notes.txt, @@, -TODO: fix, +READY: fix, *** End Patch. Reply only edited."
  "$CV" index >"$WORK/index.log" 2>&1
  "$CV" task decide "hook fixture choice" --for corpus:hooks --default left --option right >"$WORK/decision.log"
  "$CV" hooks install codex >"$WORK/install.log"
  HOOK_ARGS=(--dangerously-bypass-hook-trust)
  # Keep raw live payload/output evidence next to the scratch store, then scrub before copying.
  python3 -I - "$CODEX_HOME/hooks.json" "$HERE/hook_capture.py" "$WORK/hooks.jsonl" <<'PYHOOK'
import json, shlex, sys
path, capture, log = sys.argv[1:]
with open(path) as source:
    settings = json.load(source)
for groups in settings["hooks"].values():
    for group in groups:
        for handler in group["hooks"]:
            command = handler["command"]
            assert command.endswith(" --harness codex"), command
            handler["command"] = shlex.join(["python3", "-I", capture, log]) + " " + command
with open(path, "w") as output:
    json.dump(settings, output)
PYHOOK
  run "Do not use tools. Say the number of cv decisions owed that the hooks reported, and one earlier session id."
  id="$(last_id)"
  run -c model_auto_compact_token_limit=200 resume "$id" "Do not use tools. Say whether context compacted and which cv command reads the lost context."
  run "Use tools.apply_patch through functions.exec with this exact patch (one line per comma): *** Begin Patch, *** Update File: notes.txt, @@, -READY: fix, +DONE: fix, *** End Patch. If permission is denied, report denied and stop. Do not use any other tool."
  run --enable multi_agent "Spawn exactly one sub-agent with task 'reply with the single word ok', wait for it, and report its answer. Do not retry or follow up."
  python3 -I "$HERE/check_hooks.py" "$WORK/hooks.jsonl" "$CLUSTERVISION_HOME/lanes.jsonl" "$CV"
  [ "$(cat notes.txt)" = "READY: fix" ] || { echo "guarded patch was applied" >&2; exit 1; }
  SCEN+=("hooks")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"

# Copy the store as Codex wrote it: the rollouts and the jsonl indexes. Not config, auth, logs or
# caches, and not the sqlite projections (state_5 alone is ~250 KB vacuumed; cv reads none of them).
mkdir -p "$OUT/.codex"
cp -R "$CODEX_HOME/sessions" "$OUT/.codex/"
for f in session_index.jsonl history.jsonl; do
  [ -e "$CODEX_HOME/$f" ] && cp "$CODEX_HOME/$f" "$OUT/.codex/"
done
[ -d "$CODEX_HOME/archived_sessions" ] && cp -R "$CODEX_HOME/archived_sessions" "$OUT/.codex/"
if want hooks; then cp "$WORK/hooks.jsonl" "$OUT/hooks.jsonl"; fi
corpus_scrub "$OUT"
GENERATOR_DIRTY=false
[ -z "$(git -C "$HERE" status --porcelain -- . ../key_proxy.py ../lib.sh)" ] || GENERATOR_DIRTY=true
corpus_manifest "$OUT" \
  "harness=\"codex\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through key_proxy.py --translate responses\"" \
  "codex_model_profile=\"$PROFILE (the slug Codex recorded; it picks Codex's tool set, not the model)\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\"" "generator_dirty=$GENERATOR_DIRTY"
corpus_size_check "$OUT"
