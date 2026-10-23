#!/usr/bin/env bash
# Root-operated landing: dispatch exact candidate CI, poll every 60s, then CAS main.
# Usage: tools/land-ci.sh <origin-branch>
# Requires gh authentication, Python 3, and a clean candidate already on origin.
set -euo pipefail
fail() { echo "land-ci: $*" >&2; exit 1; }
[[ $# == 1 ]] || fail 'usage: land-ci.sh <origin-branch>'
branch=$1 workflow=ci.yml
git check-ref-format "refs/heads/$branch" >/dev/null || fail 'invalid branch'
[[ $branch != main ]] || fail 'candidate must be a separate branch'
remote_sha() {
    local result
    result=$(git ls-remote --exit-code origin "refs/heads/$1")
    [[ $result =~ ^([0-9a-f]{40})[[:space:]]refs/heads/ ]] || fail "missing remote branch: $1"
    printf '%s\n' "${BASH_REMATCH[1]}"
}
sha=$(remote_sha "$branch")
base=$(remote_sha main)
git fetch --no-tags origin "$sha" "$base"
git merge-base --is-ancestor "$base" "$sha" || fail 'candidate is not a fast-forward of main'
# Record existing IDs before dispatch: no old successful run can qualify.
before=$(gh run list --workflow "$workflow" --branch "$branch" --event workflow_dispatch --limit 100 --json databaseId)
[[ $(remote_sha "$branch") == "$sha" ]] || fail 'candidate moved before dispatch'
gh workflow run "$workflow" --ref "$branch"
run_id=
# Bounded to 24h, including queue time. Each observation is one minute apart.
for ((attempt=0; attempt<1440; attempt++)); do
    [[ $(remote_sha "$branch") == "$sha" ]] || fail 'candidate moved while waiting for CI'
    [[ $(remote_sha main) == "$base" ]] || fail 'main moved while waiting for CI; retry against new main'
    runs=$(gh run list --workflow "$workflow" --branch "$branch" --event workflow_dispatch --limit 100 --json databaseId,headSha,status,conclusion,url)
    selected=$(python3 -c '
import json, sys
old={r["databaseId"] for r in json.loads(sys.argv[1])}
runs=[r for r in json.loads(sys.argv[2]) if r["databaseId"] not in old]
if len(runs)>1: sys.exit("ambiguous concurrent dispatches; refusing to land")
if runs:
    r=runs[0]
    if r["headSha"] != sys.argv[3]: sys.exit("dispatched run has a different candidate SHA")
    print(str(r["databaseId"])+" "+r["status"]+" "+str(r["conclusion"] or "pending"))
' "$before" "$runs" "$sha")
    if [[ -n $selected ]]; then
        read -r observed status conclusion <<< "$selected"
        [[ -z $run_id || $run_id == "$observed" ]] || fail 'CI run identity changed'
        run_id=$observed
        printf 'CI %s: %s %s (%s)\n' "$run_id" "$status" "$conclusion" "$sha"
        if [[ $status == completed ]]; then
            [[ $conclusion == success ]] || fail "CI concluded $conclusion"
            break
        fi
    fi
    sleep 60
done
[[ ${status:-} == completed && ${conclusion:-} == success ]] || fail 'CI timed out'
# Query the selected run directly as a final defense against stale list results.
verified=$(gh run view "$run_id" --json headSha,status,conclusion,event --jq '[.headSha,.status,.conclusion,.event] | join(" ")')
[[ $verified == "$sha completed success workflow_dispatch" ]] || fail 'final CI identity/status check failed'
[[ $(remote_sha "$branch") == "$sha" ]] || fail 'candidate moved after CI'
# The lease rejects a race on main. Because base is an ancestor of sha, this CAS is ff-only.
git push --force-with-lease="refs/heads/main:$base" origin "$sha:refs/heads/main"
printf 'Landed %s on main after CI run %s\n' "$sha" "$run_id"
