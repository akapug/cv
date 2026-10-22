# tools/harness-corpus/lib.sh — sourced by every <harness>/generate.sh. Bash only.
#
#   corpus_work                  -> sets WORK (a fresh temp dir named cv-corpus.<lane>.XXXXXX, the lane
#                                   being $CORPUS_LANE or the harness dir) and CORPUS_HOME=$WORK/home
#                                   (the throwaway $HOME; the project lives at $CORPUS_HOME/project)
#   corpus_guard VAR...          -> refuse unless each named env var is set, non-empty, inside $WORK,
#                                   and not ember's real home or anything under it
#   corpus_proxy KEYFILE TRANSLATOR UPSTREAM [key_proxy.py args...]
#                                -> starts key_proxy.py (which alone reads KEYFILE) on a free port with
#                                   --max-calls $MAX_CALLS (default 40), logging to $CALLS_LOG (default
#                                   $WORK/calls.jsonl); sets PORT, PROXY_URL (http://127.0.0.1:$PORT),
#                                   PROXY_PID, and records KEYFILE for the scrub's literal-key check
#   corpus_proxy_stop            -> stops it (safe to call twice; put it in the EXIT trap)
#   corpus_calls                 -> the number of model calls the proxy forwarded
#   corpus_scrub DIR [--also FROM=TO ...]
#                                -> rewrite $WORK paths to /home/corpus in every file (sqlite too),
#                                   then fail on anything that looks like a secret or a real path
#   corpus_size_check DIR        -> warn over 300 KB, fail over 1.5 MB
#   corpus_manifest DIR k=v...   -> write DIR/MANIFEST.toml

CORPUS_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REAL_HOME="$(cd ~ && pwd -P)"

corpus_work() {
  # Named per lane, so a lane cleaning up after itself can name exactly its own dirs.
  local lane="${CORPUS_LANE:-$(basename "$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)")}"
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/cv-corpus.$lane.XXXXXX")"
  WORK="$(cd "$WORK" && pwd -P)"
  CORPUS_HOME="$WORK/home"
  mkdir -p "$CORPUS_HOME/project"
  export WORK CORPUS_HOME
}

corpus_guard() {
  local var val real
  for var in "$@"; do
    val="${!var:-}"
    [ -n "$val" ] || { echo "refusing: \$$var is unset" >&2; exit 1; }
    # Resolve existing symlinks even when the final directory does not exist. A refusal
    # must happen before mkdir: creating an outside path already violates this guard.
    real="$(python3 -I -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$val")" || exit 1
    case "$real" in
      "$WORK"/*|"$WORK") ;;
      *) echo "refusing: \$$var=$real is outside the throwaway dir $WORK" >&2; exit 1 ;;
    esac
    case "$real/" in
      "$REAL_HOME"/*) case "$WORK" in "$REAL_HOME"/*) ;; *) echo "refusing: \$$var under the real home" >&2; exit 1 ;; esac ;;
    esac
    mkdir -p "$val" || exit 1
  done
}

corpus_proxy() {
  local keyfile="$1" translator="$2" upstream="$3" _
  shift 3
  [ -r "$keyfile" ] && [ -s "$keyfile" ] || { echo "no key file $keyfile (or it is empty)" >&2; exit 1; }
  CORPUS_KEY_FILES="${CORPUS_KEY_FILES:+$CORPUS_KEY_FILES:}$keyfile"
  CALLS_LOG="${CALLS_LOG:-$WORK/calls.jsonl}"
  rm -f "$WORK/proxy.port"
  python3 -I "$CORPUS_LIB_DIR/key_proxy.py" --translate "$translator" --upstream "$upstream" --key-file "$keyfile" \
    --log "$CALLS_LOG" --port-file "$WORK/proxy.port" --max-calls "${MAX_CALLS:-40}" "$@" &
  PROXY_PID=$!
  for _ in $(seq 100); do [ -s "$WORK/proxy.port" ] && break; kill -0 "$PROXY_PID" 2>/dev/null || break; sleep 0.05; done
  [ -s "$WORK/proxy.port" ] || { echo "key_proxy.py ($translator) did not start" >&2; exit 1; }
  PORT="$(cat "$WORK/proxy.port")"
  PROXY_URL="http://127.0.0.1:$PORT"
  export CORPUS_KEY_FILES CALLS_LOG PORT PROXY_URL
}

corpus_proxy_stop() {
  [ -z "${PROXY_PID:-}" ] || kill "$PROXY_PID" 2>/dev/null || true
  PROXY_PID=""
}

corpus_calls() {
  if [ -f "${CALLS_LOG:-}" ]; then grep -c '"refused": false' "$CALLS_LOG" || true; else echo 0; fi
}

corpus_scrub() {
  CORPUS_REAL_HOME="$REAL_HOME" python3 -I "$CORPUS_LIB_DIR/scrub.py" --work "$WORK" --home "$CORPUS_HOME" --dir "$@"
}

corpus_size_check() {
  local kb
  # Bytes as git stores them, not `du` blocks: a store of 150 small files is mostly block padding.
  kb="$(python3 -I -c 'import os,sys; print(sum(os.path.getsize(os.path.join(d,f)) for d,_,fs in os.walk(sys.argv[1]) for f in fs) // 1024)' "$1")"
  echo "corpus size: ${kb} KB ($1)"
  # 1.5 MB: Claude Code 2.1.300+ writes a request ledger (every request's prompt, tool schemas and
  # messages, content-addressed) that nearly doubles a transcript; the warning line is the target.
  [ "$kb" -le 1536 ] || { echo "over the 1.5 MB hard cap — trim the scenario prompts" >&2; exit 1; }
  [ "$kb" -le 300 ] || echo "warning: over the 300 KB target" >&2
}

corpus_manifest() {
  local dir="$1"; shift
  {
    echo "# Written by tools/harness-corpus. Never regenerate a committed version in place."
    local kv
    for kv in "$@"; do
      printf '%s = %s\n' "${kv%%=*}" "${kv#*=}"
    done
  } >"$dir/MANIFEST.toml"
}
