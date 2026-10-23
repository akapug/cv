#!/usr/bin/env python3
"""One owned cold-WAL CLI/MCP lifecycle control, with an independent output oracle.

POSIX only (signals and ps). Fixture generation happens before starting measured children.
This checks named payload lifetime and source/output fidelity, not timings or physical blocks.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import sys
import subprocess
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from perf_fixture_sqlite import generate_sqlite


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def identity(path):
    stat = path.stat()
    return [stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns]


def cold_wal(source):
    """Freeze committed WAL frames without changing any logical row or leaving live SHM."""
    source = Path(source)
    connection = sqlite3.connect(source)
    try:
        connection.execute("PRAGMA journal_mode=WAL")
        version = connection.execute("PRAGMA user_version").fetchone()[0]
        connection.execute(f"PRAGMA user_version={version}")
        database = source.read_bytes()
        wal = Path(str(source) + "-wal").read_bytes()
        assert len(wal) > 32, "fixture needs committed WAL frames, not just a WAL header"
    finally:
        connection.close()
    source.write_bytes(database)
    Path(str(source) + "-wal").write_bytes(wal)


def children(pid):
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,command="], text=True)
    return [int(parts[0]) for row in rows.splitlines()
            if len(parts := row.split(None, 2)) >= 2 and int(parts[1]) == pid]


def process_identity(pid):
    result = subprocess.run(["ps", "-p", str(pid), "-o", "lstart=,comm="], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, help="independent legacy binary; required for new scratch state")
    parser.add_argument("--reuse-state", action="store_true", help="reuse this control's owned fixture and normal.json oracle")
    args = parser.parse_args()
    root, binary = args.out.resolve(), args.binary.resolve(strict=True)
    if not args.reuse_state:
        if not args.baseline:
            parser.error("--baseline is required when creating new scratch state")
        root.mkdir(parents=True, exist_ok=False)
        fixture = generate_sqlite("goose", root / "goose/data/sessions", 8_000_000, 8192, True, "history")
        cold_wal(fixture["path"])
        (root / "fixture.json").write_text(json.dumps(fixture, indent=2) + "\n")
        for directory in ("home", "catalog", "temporary", "home/.local/share", "home/.config", "home/.cache"):
            (root / directory).mkdir(parents=True, exist_ok=True)
    fixture = json.loads((root / "fixture.json").read_text())
    source, temporary = Path(fixture["path"]), root / "temporary"
    # Cold copies stage beside the writable catalog, never in TMPDIR.
    owners = root / "catalog" / "cv-sqlite-owners-v1"
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ}
    env.update(HOME=str(root / "home"), CLUSTERVISION_HOME=str(root / "catalog"), TMPDIR=str(temporary),
               GOOSE_PATH_ROOT=str(root / "goose"), XDG_DATA_HOME=str(root / "home/.local/share"),
               XDG_CONFIG_HOME=str(root / "home/.config"), XDG_CACHE_HOME=str(root / "home/.cache"),
               RAYON_NUM_THREADS="1")
    originals = {
        suffix: (digest(path), identity(path)) if path.exists() else None
        for suffix in ("", "-wal", "-shm", "-journal")
        for path in [Path(str(source) + suffix)]
    }
    original, original_identity = originals[""]
    assert originals["-wal"] is not None, "lifecycle fixture must contain WAL frames"
    command = ["doctor", "goose:" + fixture["id"], "--json"]

    def guards():
        for suffix, before in originals.items():
            path = Path(str(source) + suffix)
            after = (digest(path), identity(path)) if path.exists() else None
            assert after == before, f"source changed: {path}"

    def own(pid):
        with (root / "owned-pids.txt").open("a") as pids:
            pids.write(f"{pid}\n")

    def payloads():
        return [{"path": str(path.relative_to(root)), "bytes": path.stat().st_size}
                for path in sorted(owners.rglob("store.db*")) if path.is_file()]

    def wait_copy(process):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            assert process.poll() is None, "process completed before cancellation witness"
            for path in owners.rglob("store.db"):
                try:
                    if path.stat().st_size >= fixture["physical_input_bytes"]:
                        return str(path.relative_to(root))
                except FileNotFoundError:
                    pass
            time.sleep(.001)
        raise AssertionError("no completed private copy")

    def normal():
        process = subprocess.run([str(binary), *command], env=env, cwd=root, capture_output=True, timeout=30)
        assert process.returncode == 0, process.stderr
        assert process.stdout == (root / "normal.json").read_bytes()
        assert not payloads(), payloads()
        guards()
        return hashlib.sha256(process.stdout).hexdigest()

    if not args.reuse_state:
        baseline_temporary = root / "baseline-temporary"
        baseline_temporary.mkdir()
        process = subprocess.run([str(args.baseline.resolve(strict=True)), *command],
                                 env={**env, "TMPDIR": str(baseline_temporary)}, cwd=root,
                                 capture_output=True, check=True, timeout=30)
        (root / "normal.json").write_bytes(process.stdout)
        guards()
    report = {"binary": str(binary), "binary_sha256": digest(binary), "source_sha256": original,
              "source_identity": original_identity, "normal_output_sha256": normal(), "standalone": []}
    for sig in (signal.SIGINT, signal.SIGKILL):
        process = subprocess.Popen([str(binary), *command], env=env, cwd=root,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        own(process.pid)
        try:
            witness = wait_copy(process)
            process.send_signal(sig)
            out, err = process.communicate(timeout=10)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()
        leftovers = payloads()
        assert leftovers
        guards()
        output_sha = normal()
        report["standalone"].append({"signal": sig.name, "pid": process.pid, "returncode": process.returncode,
            "completed_copy_witness": witness, "after_exit_payloads": leftovers,
            "after_next_operation_payloads": payloads(), "next_output_sha256": output_sha,
            "stdout_bytes": len(out), "stderr": err.decode()})

    process = subprocess.Popen([str(binary), "mcp"], env=env, cwd=root, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    own(process.pid)
    def send(value):
        process.stdin.write((json.dumps(value) + "\n").encode())
        process.stdin.flush()
    started, started_identities = [], {}
    failed = True
    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
              "params": {"name": "doctor", "arguments": {"id": "goose:" + fixture["id"]}}})
        witness = wait_copy(process)
        started = children(process.pid)
        assert len(started) == 1, "expected one generated child alive at snapshot witness"
        # It cannot naturally finish after this witness. Disconnect must kill and reap it.
        started_identities = {pid: process_identity(pid) for pid in started}
        assert all(started_identities.values()), "generated child disappeared before stop"
        own(started[0])
        os.kill(started[0], signal.SIGSTOP)
        process.stdout.close()
        process.stdout = None
        send({"jsonrpc": "2.0", "id": 2, "method": "ping"})
        process.stdin.close()
        process.stdin = None
        _, err = process.communicate(timeout=15)
        assert process.returncode == 0, err
        remaining = []
        for child in started:
            try:
                os.kill(child, 0)
            except ProcessLookupError:
                pass
            else:
                remaining.append(child)
        assert not remaining, remaining  # Check before failure cleanup could conceal a defect.
        assert not payloads(), payloads()
        guards()
        failed = False
    finally:
        if failed:
            # A stopped generated child can survive an unexpectedly dead parent. Retain its
            # observed start/command identity and clean up only this control's known child.
            owned = set(children(process.pid))
            owned.update(pid for pid, stamp in started_identities.items()
                         if stamp is not None and process_identity(pid) == stamp)
            for child in owned:
                own(child)
                try:
                    os.kill(child, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if process.stdin is not None:
                process.stdin.close()
                process.stdin = None
            if process.stdout is not None:
                process.stdout.close()
                process.stdout = None
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        if process.poll() is None:
            process.kill()
        process.wait()
    report["mcp_disconnect"] = {"pid": process.pid, "returncode": process.returncode,
        "generated_children_at_witness": started, "child_stopped_before_disconnect": True,
        "children_after_server_exit": remaining, "completed_copy_witness": witness,
        "payloads_after_joined_handlers": payloads(), "stderr": err.decode()}
    report["final_normal_output_sha256"] = normal()
    report["final_bookkeeping"] = [{"path": str(path.relative_to(root)), "bytes": path.stat().st_size}
                                   for path in sorted(owners.rglob("*")) if path.is_file()]
    assert not any(temporary.iterdir()), "a copy staged in TMPDIR"
    assert owners.is_dir() and not owners.is_symlink()
    assert [path.name for path in owners.iterdir()] == ["gate"], "abandoned payload or metadata"
    gate = json.loads((owners / "gate").read_bytes())
    assert gate["magic"] == "cv-sqlite-owners-v1"
    assert gate["root"] == {"volume": owners.stat().st_dev, "file": owners.stat().st_ino}
    report["limits"] = ["standalone abrupt exit reclamation is eventual at a later operation boundary",
        "MCP closed-output cancellation kills and reaps a stopped generated child before request cleanup",
        "logical named payload extents only, not physical APFS blocks or timing/RSS",
        "owned fixture/home/catalog/TMPDIR only (copies stage in the catalog); source and WAL hashes/identities, auxiliary absence and exact output preserved",
        "fixed recognized gate metadata may remain; no abandoned store.db payloads"]
    (root / "candidate-lifecycle.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
