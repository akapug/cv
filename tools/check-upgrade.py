#!/usr/bin/env python3
"""Exercise 0.16.1 -> candidate catalogs over every session-bearing corpus version.

Run on the build host with two already-built binaries. Stores are copied into a new scratch
directory; no real home or committed corpus file is opened by either binary. Retain scratch
on success or failure for diagnosis. No transcript dumps are saved: show outputs are hashed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import sqlite3
import subprocess
import tempfile
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(binary, env, *args, timeout=600):
    result = subprocess.run([str(binary), *args], env=env, capture_output=True,
                            text=True, timeout=timeout)
    require(result.returncode == 0,
            f"{binary.name} {' '.join(args)}: exit {result.returncode}: {result.stderr[-2000:]}")
    return result.stdout


def show_hash(binary, env, selector):
    # stdout streams directly into the digest. Only small diagnostics go to an owned temp file.
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen([str(binary), "show", selector, "--json"], env=env,
                                   stdout=subprocess.PIPE, stderr=errors)
        pidfile = Path(env["TMPDIR"]) / "show.pid"
        pidfile.write_text(str(process.pid))
        digest, size = hashlib.sha256(), 0
        deadline = time.monotonic() + 600
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(process.stdout, selectors.EVENT_READ)
                while True:
                    require(time.monotonic() < deadline, f"show {selector}: timed out")
                    if not ready.select(min(10, max(0, deadline - time.monotonic()))):
                        continue
                    chunk = os.read(process.stdout.fileno(), 65536)
                    if not chunk:
                        break
                    digest.update(chunk)
                    size += len(chunk)
            code = process.wait(timeout=max(0, deadline - time.monotonic()))
        finally:
            process.stdout.close()
            if process.poll() is None and int(pidfile.read_text()) == process.pid:
                process.kill()
                process.wait()
        errors.seek(0)
        require(code == 0 and size > 0, f"show {selector}: exit {code}: {errors.read(2000)!r}")
        return size, digest.hexdigest()


def environment(case, home, catalog):
    # Deliberately inherit no harness-root overrides or user configuration.
    return {"PATH": os.environ["PATH"], "HOME": str(home), "NO_COLOR": "1",
            "CLUSTERVISION_HOME": str(catalog), "TMPDIR": str(case / "tmp"),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local/share"),
            "XDG_CACHE_HOME": str(case / "cache"),
            "CLAUDE_CONFIG_DIR": str(home / ".claude"),
            "CODEX_HOME": str(home / ".codex"),
            "CURSOR_CONFIG_DIR": str(home / ".cursor"),
            "CURSOR_USER_DIR": str(home / ".config/Cursor/User"),
            "KIMI_CODE_HOME": str(home / ".kimi-code"),
            "HERMES_HOME": str(home / ".hermes"),
            "RAYON_NUM_THREADS": "4"}


def stage(version, case):
    home = case / "home"
    shutil.copytree(version, home)
    for directory in (case / "tmp", case / "cache", home / ".config", home / ".local/share"):
        directory.mkdir(parents=True, exist_ok=True)
    # Corpus adapters name these roots explicitly; reproduce their native detected locations.
    harness = version.parent.name
    if harness == "cursor" and (home / "Cursor").is_dir():
        shutil.move(str(home / "Cursor"), home / ".config/Cursor")
    if harness == "goose":
        (home / ".local/share/goose").mkdir()
        shutil.move(str(home / "sessions"), home / ".local/share/goose/sessions")
    if harness == "openclaw":
        (home / ".openclaw").mkdir()
        for name in ("agents", "cron", "subagents"):
            if (home / name).exists():
                shutil.move(str(home / name), home / ".openclaw" / name)
    return home


def rows(binary, env, expected, baseline=False):
    result = json.loads(run(binary, env, "ls", "--json"))
    require(isinstance(result, list), "ls did not return rows")
    actual = {(row["harness"], row["id"]) for row in result}
    require(len(actual) == len(result), "duplicate discovered session identities")
    require(actual <= expected if baseline else actual == expected,
            f"discovery differs from corpus: missing={expected-actual}, extra={actual-expected}")
    return sorted(result, key=lambda row: (row["harness"], row["id"]))


def index(binary, env, unchanged=False):
    result = json.loads(run(binary, env, "index", "--strict", "--json"))
    require(result.get("unreadable") == [], f"unreadable upgrade sources: {result}")
    counts = [result.get(key) for key in ("indexed", "changed", "removed", "errors")]
    require(all(type(count) is int for count in counts), f"missing index counts: {result}")
    _, changed, removed, errors = counts
    require(errors == 0, f"index publication errors: {result}")
    if unchanged:
        require((changed, removed) == (0, 0), f"unchanged pass performed work: {result}")
    return changed


def event_digest(catalog):
    # Compare the candidate's logical event rows, excluding allocation/order-specific row IDs.
    with sqlite3.connect(f"file:{catalog / 'catalog.db'}?mode=ro", uri=True) as connection:
        digest, count = hashlib.sha256(), 0
        query = """SELECT s.harness,s.session_id,s.parent_id,s.agent_id,s.workflow,
                          e.msg_idx,e.ts,e.kind,e.tool,t.target,e.detail
                   FROM events e JOIN event_sessions s ON e.session=s.id
                   LEFT JOIN event_targets t ON e.target_id=t.id
                   ORDER BY 1,2,3,4,5,6,7,8,9,10,11"""
        for row in connection.execute(query):
            digest.update(json.dumps(row, separators=(",", ":")).encode())
            digest.update(b"\n")
            count += 1
        return count, digest.hexdigest()


def check_version(baseline, candidate, version, case):
    summary = json.loads((version / "summary.json").read_text())
    harness = version.parent.name
    expected = {(harness, row["id"]) for row in summary["sessions"]}
    require(bool(expected), f"empty session-bearing corpus {version}")
    home = stage(version, case)
    upgraded = environment(case, home, case / "upgraded")
    # 0.16.1 predates index --json/--strict and some current stores (notably Cursor CLI).
    # Report those as newly supported inputs instead of pretending the baseline read them.
    old_rows = rows(baseline, upgraded, expected, baseline=True)
    old_output = run(baseline, upgraded, "index")
    old_count = re.search(r"indexed (\d+) top-level session", old_output)
    require(old_count is not None and (int(old_count[1]) > 0 or not old_rows),
            "baseline did not populate its index")
    # The baseline counted every session; the candidate certifies counts itself. Its first listing
    # over the old catalog must leave exactly the counts a fresh listing leaves uncounted (null),
    # never carry the baseline's numbers forward.
    counts = lambda listed: {(row["harness"], row["id"]): row["message_count"] for row in listed}
    first = counts(rows(candidate, upgraded, expected))
    fresh = counts(rows(candidate, environment(case, home, case / "fresh"), expected))
    require(first == fresh, f"upgraded first listing counts differ from a fresh one: {first} != {fresh}")
    deferred = sum(count is None for count in first.values())
    migrated = index(candidate, upgraded)
    upgraded_rows = rows(candidate, upgraded, expected)
    uncounted = [(row["harness"], row["id"]) for row in upgraded_rows if type(row["message_count"]) is not int]
    require(not uncounted, f"index left counts uncertified: {uncounted}")
    hashes = {selector: show_hash(candidate, upgraded, selector)
              for selector in sorted(f"{h}:{sid}" for h, sid in expected)}
    upgraded_events = event_digest(case / "upgraded")
    index(candidate, upgraded, unchanged=True)
    index(candidate, upgraded, unchanged=True)
    clean = environment(case, home, case / "clean")
    index(candidate, clean)
    require(rows(candidate, clean, expected) == upgraded_rows, "upgraded session rows differ from clean candidate")
    require(event_digest(case / "clean") == upgraded_events, "upgraded events differ from clean candidate")
    for selector, expected_hash in hashes.items():
        require(show_hash(candidate, clean, selector) == expected_hash,
                f"upgraded show differs from clean candidate: {selector}")
    index(candidate, clean, unchanged=True)
    return {"corpus": str(version.parent.name + "/" + version.name), "sessions": len(expected),
            "baseline_sessions": len(old_rows), "newly_discovered": len(expected) - len(old_rows),
            "deferred_counts": deferred, "migrated": migrated, "events": upgraded_events[0], "unchanged_passes": 2,
            "show_bytes_hashed": sum(size for size, _ in hashes.values())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--corpus", type=Path, default=Path("crates/cv-core/tests/corpus"))
    parser.add_argument("--scratch-parent", type=Path)
    args = parser.parse_args()
    baseline, candidate = args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)
    versions = sorted(path.parent for path in args.corpus.resolve(strict=True).glob("*/*/summary.json"))
    require(bool(versions), "no session corpus versions found")
    manifests = {path.parent for path in args.corpus.resolve().glob("*/*/MANIFEST.toml")}
    require(manifests == set(versions), f"session corpus manifests/goldens differ: {manifests ^ set(versions)}")
    scratch = Path(tempfile.mkdtemp(prefix="cv-upgrade-", dir=args.scratch_parent))
    print(f"Upgrade scratch retained: {scratch}", flush=True)
    (scratch / "tmp").mkdir()
    env = environment(scratch, scratch / "home", scratch / "version-home")
    version = run(baseline, env, "--version").strip()
    require(re.search(r"\b0\.16\.1(?:\s|$)", version), f"baseline must be 0.16.1, got {version!r}")
    print(f"Baseline: {version}; candidate: {run(candidate, env, '--version').strip()}", flush=True)
    total = 0
    for version in versions:
        result = check_version(baseline, candidate, version, scratch / version.parent.name / version.name)
        total += result["sessions"]
        print(json.dumps(result, sort_keys=True), flush=True)
    print(f"Upgrade passed: {len(versions)} corpus versions, {total} session instances.")


if __name__ == "__main__":
    main()
