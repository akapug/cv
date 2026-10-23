#!/usr/bin/env python3
"""Isolated SQLite allocator controls on three existing selected-payload schemas.

Each measurement gets one new process; fixture generation and source hashing are outside
its stage. Built-in SQLite status reports peaks/live allocation counts, not cumulative work.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
from perf_fixture_sqlite import generate_sqlite

CASES = (("opencode", "selected"), ("cursor", "selected-ide"), ("goose", "selected"))


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def identity(path):
    stat = path.stat()
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binaries = {k: p.resolve(strict=True) for k, p in (("probe", args.probe), ("baseline", args.baseline))}
    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=False)
    home, catalog, temporary = [root / name for name in ("home", "catalog", "temporary")]
    for path in (home, catalog, temporary):
        path.mkdir()
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
           "CLUSTERVISION_HOME": str(catalog), "TMPDIR": str(temporary),
           "XDG_DATA_HOME": str(home / ".local/share"), "XDG_CONFIG_HOME": str(home / ".config"),
           "XDG_CACHE_HOME": str(home / ".cache"), "LC_ALL": "C", "RAYON_NUM_THREADS": "1"}
    results = []
    for harness, shape in CASES:
        for size in (8_000_000, 32_000_000):
            for scale, records in (("giant", 1), ("rows", size // 125_000)):
                destination = root / f"{harness}-{scale}-{size}"
                destination.mkdir()
                fixture = generate_sqlite(harness, destination, size, records, True, shape)
                source = Path(fixture["path"])
                source_digest, source_identity = digest(source), identity(source)
                common = [harness, fixture["id"], fixture["path"], "--root", fixture["root"], "--mode", "lazy"]
                def run(label, operation, extra=()):
                    child = subprocess.run([str(binaries[label]), operation, *common, *extra],
                                           env=env, cwd=root, capture_output=True, text=True, check=True)
                    assert not child.stderr, child.stderr
                    result = json.loads(child.stdout)
                    result.pop("wall_seconds", None)
                    assert digest(source) == source_digest and identity(source) == source_identity
                    assert not list(temporary.iterdir()), "private source backing leaked"
                    return result
                control = run("probe", "baseline", ["--sqlite-heap"])
                result = run("probe", "discard", ["--sqlite-heap"])
                counters = result.get("sqlite_native_heap")
                assert counters and counters["accounting_enabled_and_verified"]
                assert counters["verified_allocation_bytes"] >= 4096
                assert counters["peak_live_bytes"] >= counters["baseline_live_bytes"]
                assert counters["largest_allocation_request_bytes"] > 0
                assert "cumulative_allocation_requests" not in counters, "unmeasured counter reported"
                assert "cumulative_requested_bytes" not in counters, "unmeasured counter reported"
                assert result["messages"] == fixture["expected_messages"]
                assert result["build_features"] == {"mmap": True, "probe_heap": True}
                before = run("baseline", "hash", ["--raw-order"])
                after = run("probe", "hash", ["--raw-order", "--sqlite-heap"])
                assert before["messages"] == after["messages"] == fixture["expected_messages"]
                assert before["detail"] == after["detail"], "preserved-order output/metadata differ"
                results.append({"fixture": fixture, "source_sha256": source_digest,
                                "empty_stage": control, "selected_discard": result,
                                "fidelity": after["detail"]})
                print(f"{harness}/{scale}/{size}: Rust peak {result['requested_rust_heap']['peak_above_baseline_bytes']}; "
                      f"SQLite peak {counters['peak_above_baseline_bytes']}; "
                      f"largest native request {counters['largest_allocation_request_bytes']}", flush=True)
    summary = {"base_revision": "7c73b199", "operation": "native lazy selected adapter stream/discard",
               "binary_sha256": {label: digest(path) for label, path in binaries.items()},
               "results": results,
               "limits": ["SQLite counters include its allocator/page-cache overhead, exclude underlying system allocator overhead",
                          "SQLite built-in status provides largest request and live/peak allocations; no cumulative request count/bytes",
                          "global process counters include every SQLite connection used in the stage",
                          "Rust requested heap is separate; mapped arena/source pages, filesystem cache, stacks and native allocator overhead excluded",
                          "source hashing primes file cache outside stage; no cold-I/O or elapsed-time claim",
                          "hash fidelity materializes messages and is excluded from discard heap measurements",
                          "incremental blob-readable native rowid schemas only; no universal virtual/WITHOUT ROWID fallback bound"]}
    (root / "SQLITE-NATIVE-HEAP.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
