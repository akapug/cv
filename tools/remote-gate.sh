#!/usr/bin/env bash
# Usage: tools/remote-gate.sh <origin-branch-or-full-sha> <lane>
# A fast remote preflight, NOT the full CI gate. land-ci.sh requires Actions.
# Prerequisites on persvati: pinned Rust toolchain, sccache, nextest, wasm target.
set -euo pipefail
fail() { echo "remote-gate: $*" >&2; exit 1; }
if [[ ${1:-} != --worker ]]; then
    [[ $# == 2 ]] || fail 'usage: remote-gate.sh <origin-branch-or-full-sha> <lane>'
    ref=$1 lane=$2
    [[ $lane =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ ]] || fail 'invalid lane'
    if [[ $ref =~ ^[0-9a-f]{40}$ ]]; then
        sha=$ref
    else
        git check-ref-format "refs/heads/$ref" >/dev/null || fail 'invalid branch'
        git fetch --no-tags origin "refs/heads/$ref"
        sha=$(git rev-parse --verify 'FETCH_HEAD^{commit}')
    fi
    # Only validated hexadecimal/alphanumeric arguments enter the remote shell.
    exec ssh -o BatchMode=yes persvati "bash -s -- --worker $sha $lane" < "$0"
fi
shift
[[ $# == 2 && $1 =~ ^[0-9a-f]{40}$ && $2 =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ ]] || fail 'invalid worker arguments'
sha=$1 lane=$2
base=$HOME/cv-remote
mkdir -p "$base"
# Locks the worktree, its warm target, and its scratch state together. Never steal a lock.
command -v flock >/dev/null || fail 'persvati requires flock'
exec 9>"$base/lock-$lane"
flock -n 9 || fail "lane $lane is busy"
repo=$HOME/dev/cv
work=$base/wt-$lane
marker=$base/owned-$lane
cd "$repo"
git fetch --no-tags origin "$sha"
[[ $(git rev-parse --verify 'FETCH_HEAD^{commit}') == "$sha" ]] || fail 'fetched SHA differs'
if [[ -e $work ]]; then
    [[ -f $marker && $(cat "$marker") == "$work" ]] || fail "existing worktree is not owned by this script: $work"
    [[ $(git -C "$work" rev-parse --show-toplevel) == "$work" ]] || fail 'worktree mismatch'
    [[ -z $(git -C "$work" status --porcelain --untracked-files=all) ]] || fail 'worktree has existing changes'
    [[ -z $(git -C "$work" symbolic-ref -q HEAD || :) ]] || fail 'worktree has an attached branch'
    git -C "$work" checkout --detach "$sha"
else
    git worktree add --detach "$work" "$sha"
    printf '%s\n' "$work" > "$marker"
fi
cd "$work"
export CARGO_TARGET_DIR=$base/target-$lane CLUSTERVISION_HOME=$base/home-$lane
export RUSTC_WRAPPER=$HOME/.cargo/bin/sccache CARGO_BUILD_JOBS=8
export CARGO_TERM_COLOR=never RUST_BACKTRACE=1
mkdir -p "$CARGO_TARGET_DIR" "$CLUSTERVISION_HOME"
logs=$(mktemp -d "$base/log-$lane-XXXXXXXX")
failed=0 passed=0
run() {
    local name=$1; shift
    if "$@" >"$logs/$name.log" 2>&1; then
        printf 'PASS %s\n' "$name"; passed=$((passed + 1))
    else
        printf 'FAIL %s (%s)\n' "$name" "$logs/$name.log"
        tail -n 10 "$logs/$name.log"
        failed=$((failed + 1))
    fi
}
printf 'Remote preflight: %s; logs: %s\n' "$sha" "$logs"
run fmt cargo fmt --all --check
run size python3 -I tools/check-size.py
run corpus-tools python3 -I tools/harness-corpus/test_tools.py
run clippy cargo clippy --workspace --all-targets -- -D warnings
run build cargo build --workspace --all-targets
run nextest cargo nextest run --workspace --profile ci
run doctest cargo test --workspace --doc
run core-no-default cargo clippy -p clustervision-core --no-default-features --all-targets -- -D warnings
run core-wasm cargo clippy -p clustervision-core --no-default-features --target wasm32-unknown-unknown -- -D warnings
run search-no-default cargo clippy -p cv-search --no-default-features -- -D warnings
run package cargo package --workspace --exclude cv-web --exclude cv-testkit
run manual-names bash tools/check-manual-names.sh "$CARGO_TARGET_DIR/debug/cv"
run manual env CV_BIN="$CARGO_TARGET_DIR/debug/cv" mdbook build manual -d "$logs/manual"
printf 'Preflight: %s passed, %s failed. NOT full CI.\n' "$passed" "$failed"
echo 'Actions still required: provenance, parse/heap/positioned-reader gates, dist plan, wasm-pack, desktop, web (and workflow changes).'
[[ $failed == 0 ]]
