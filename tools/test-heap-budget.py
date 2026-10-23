#!/usr/bin/env python3
"""Check that the diagnostic budget driver rejects missing instrumentation and failed probes."""

import importlib.util
import json
from pathlib import Path
import sys
import subprocess
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("heap_budget", Path(__file__).with_name("check-heap-budget.py"))
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)


class DriverTests(unittest.TestCase):
    def test_event_fixtures_keep_close_reopen_and_native_shapes(self):
        with tempfile.TemporaryDirectory() as directory:
            for shape in ("unfinished", "open-close", "native"):
                source = Path(directory) / f"{shape}.jsonl"
                harness, messages, expected = budget.event_fixture(source, shape, 4)
                rows = [json.loads(line) for line in source.read_text().splitlines()]
                self.assertEqual(len(rows), messages)
                self.assertEqual(len(expected), 8 if shape == "native" else 5 if shape == "open-close" else 4)
                if shape == "open-close":
                    ids = [row["message"]["content"][0]["tool_use_id"] for row in rows[4:8]]
                    self.assertEqual(ids, [f"call-{i}" for i in reversed(range(4))])
                    self.assertEqual(rows[-1]["message"]["content"][0]["type"], "tool_use")
                elif shape == "native":
                    self.assertEqual(harness, "codex")
                    self.assertEqual(rows[1]["payload"]["item"]["type"], "FileChange")
                    self.assertEqual(len(rows[1]["payload"]["item"]["changes"]), 2)

    def test_events_gate_runs_every_shape_for_either_build(self):
        for mmap in (False, True):
            with tempfile.TemporaryDirectory() as directory, patch.object(budget, "events_case") as cases:
                budget.events("probe", Path(directory), {}, mmap=mmap)
                self.assertEqual([call.args[3] for call in cases.call_args_list],
                                 ["unfinished", "open-close", "native"])
                self.assertTrue(all(call.kwargs == {"mmap": mmap} for call in cases.call_args_list))

    def result(self, features, heap):
        return SimpleNamespace(returncode=0, stdout=json.dumps({"build_features": features, "requested_rust_heap": heap}), stderr="")

    def test_shell_doctor_rejects_owned_row_or_changed_report(self):
        row = dict(messages=1, detail=budget.SHELL_DOCTOR_EXPECTED,
                   requested_rust_heap=dict(allocation_requests=344, requested_bytes=235519),
                   sqlite_native_heap=dict(accounting_enabled_and_verified=True,
                                           peak_above_baseline_bytes=2622192,
                                           largest_allocation_request_bytes=65600))
        budget.check_shell_doctor(row, 135854)
        with self.assertRaisesRegex(AssertionError, "live Rust heap"):
            budget.check_shell_doctor(row, 100674181)
        row["sqlite_native_heap"]["largest_allocation_request_bytes"] = 33554638
        with self.assertRaisesRegex(AssertionError, "whole large SQLite cell"):
            budget.check_shell_doctor(row, 135854)
        row["detail"] = dict(messages=0)
        with self.assertRaisesRegex(AssertionError, "report changed"):
            budget.check_shell_doctor(row, 135854)

    def test_shell_fixture_matches_native_output_shape(self):
        generator = Path(__file__).with_name("perf-fixture.py")
        with tempfile.TemporaryDirectory() as directory:
            manifest = json.loads(subprocess.check_output([
                sys.executable, str(generator), "opencode", str(Path(directory) / "shell"),
                "--shape", "shell-v2", "--records", "1", "--payload-bytes", "257"], text=True))
            with sqlite3.connect(f"file:{manifest['path']}?mode=ro", uri=True) as connection:
                kind, data = connection.execute("SELECT type,data FROM session_message").fetchone()
            data = json.loads(data)
            self.assertEqual(kind, "shell")
            self.assertEqual(data["command"], "echo user-shell")
            self.assertEqual(len(data["output"]["output"].encode()), 257)
            self.assertEqual(set(data["output"]), {"output", "cursor", "size", "truncated"})
            self.assertEqual(data["metadata"], {"background": True})
            self.assertEqual(manifest["expected_messages"], 1)

    def test_clean_timing_binary_is_rejected(self):
        with patch.object(budget.subprocess, "run", return_value=self.result({"mmap": True, "probe_heap": False}, None)):
            with self.assertRaisesRegex(AssertionError, "instrumentation"):
                budget.probe("unused", [], {}, True)

    def test_missing_counters_are_rejected(self):
        with patch.object(budget.subprocess, "run", return_value=self.result({"mmap": True, "probe_heap": True}, None)):
            with self.assertRaisesRegex(AssertionError, "counters missing"):
                budget.probe("unused", [], {}, True)

    def test_failed_probe_is_rejected(self):
        with patch.object(budget.subprocess, "run", return_value=SimpleNamespace(returncode=1, stdout="", stderr="bad source")):
            with self.assertRaisesRegex(AssertionError, "probe failed.*bad source"):
                budget.probe("unused", [], {}, True)

    def test_size_dependent_growth_is_rejected(self):
        with self.assertRaisesRegex(AssertionError, "grows with source size"):
            budget.bound([budget.MIB, 4 * budget.MIB], 8 * budget.MIB, 2 * budget.MIB, "source")

    def test_facts_gate_rejects_changed_full_report(self):
        manifest = {"harness": "claude", "id": "fixture", "path": "source", "root": "root",
                    "expected_messages": 20, "expected_compactions": 4, "decoded_metadata_payload_bytes": 16,
                    "physical_input_bytes": 32}
        report = {"compactions": 4, "conversational": {"tool_results": 2}}
        facts = {"messages": 20, "detail": report}
        full = {"messages": 20, "detail": dict(report, compactions=0)}
        with patch.object(budget.subprocess, "check_output", return_value=json.dumps(manifest)), \
             patch.object(budget, "probe", side_effect=[(facts, 1024), (full, 4096)]):
            with self.assertRaisesRegex(AssertionError, "report changed"):
                budget.metadata_facts("unused", Path("unused"), {})

    def test_metadata_fixture_payload_budgets_and_native_cli_references(self):
        generator = Path(__file__).with_name("perf-fixture.py")
        with tempfile.TemporaryDirectory(prefix="cv-test-facts-") as directory:
            for harness in ("claude", "cursor", "goose"):
                for escaped in (False, True):
                    command = [sys.executable, str(generator), harness,
                               str(Path(directory) / f"{harness}-{escaped}"), "--shape", "metadata",
                               "--records", "3", "--payload-bytes", "257"]
                    if escaped:
                        command.append("--escaped")
                    manifest = json.loads(subprocess.check_output(command, text=True))
                    self.assertEqual(manifest["decoded_metadata_payload_bytes"], 257)
                    if harness == "claude":
                        rows = [json.loads(line) for line in Path(manifest["path"]).read_text().splitlines()]
                        self.assertEqual(len(rows), manifest["expected_messages"])
                        payload = sum(len(row.get("toolUseResult", {}).get("stdout", ""))
                                      + len(row.get("compactMetadata", {}).get("unusedPayload", ""))
                                      + len(row.get("attachment", {}).get("unusedPayload", "")) for row in rows)
                    elif harness == "goose":
                        with sqlite3.connect(manifest["path"]) as db:
                            rows = [json.loads(raw) for raw, in db.execute("SELECT metadata_json FROM messages")]
                            self.assertEqual(len(rows), manifest["expected_messages"])
                            payload = sum(len(operation["ignoredPayload"]) for row in rows
                                          for operation in row.get("operations", []))
                    else:
                        with sqlite3.connect(manifest["path"]) as db:
                            metadata = json.loads(bytes.fromhex(db.execute("SELECT value FROM meta WHERE key='0'").fetchone()[0]))
                            native_root = db.execute("SELECT data FROM blobs WHERE id=?", (metadata["latestRootBlobId"],)).fetchone()[0]
                            rows = []
                            for raw, in db.execute("SELECT data FROM blobs WHERE id!=?", (metadata["latestRootBlobId"],)):
                                rows.append(json.loads(raw))
                            self.assertEqual(len(rows), manifest["expected_messages"])
                            self.assertTrue(native_root.endswith(bytes([80, 1])))
                            payload = sum(len(row.get("providerOptions", {}).get("cursor", {}).get("ignoredPayload", ""))
                                          + len(row.get("providerOptions", {}).get("cursor", {}).get("highLevelToolCallResult", {}).get("output", ""))
                                          for row in rows)
                    self.assertEqual(payload, 257)

    def test_selected_gate_rejects_changed_serialized_record(self):
        manifest = {"harness": "cursor", "id": "fixture", "path": "source", "root": "root",
                    "expected_messages": 5}
        doctor = {"messages": 5, "detail": {"messages": 5}}
        full = {"detail": {"record_written": True, "serialized_record_bytes": 42, "record_sha256": "full"}}
        changed = {"detail": dict(full["detail"], record_sha256="changed")}
        for format_name, mmap in (("cursor", True), ("cursor", False), ("gemini", False)):
            with self.subTest(format=format_name, mmap=mmap), patch("builtins.print"), \
                 patch.object(budget.subprocess, "check_output", return_value=json.dumps(manifest)), \
                 patch.object(budget, "probe", side_effect=[(doctor, 0), (full, 0), (doctor, 0), (doctor, 0), (changed, 0)]) as calls:
                with self.assertRaisesRegex(AssertionError, "dataset differs"):
                    budget.selected_payloads("unused", Path("unused"), {}, [format_name], mmap=mmap)
                self.assertEqual(calls.call_args_list[-1].args[1][0], "dataset")
                self.assertTrue(all(call.args[3] == mmap for call in calls.call_args_list))

    def test_selected_codex_and_gemini_distribute_all_requested_bytes(self):
        generator = Path(__file__).with_name("perf-fixture.py")
        with tempfile.TemporaryDirectory(prefix="cv-test-selected-json-") as directory:
            for harness, shape in (("codex", "selected-response"), ("codex", "selected-events"), ("gemini", "selected"), ("gemini", "selected-jsonl")):
                manifest = json.loads(subprocess.check_output([
                    sys.executable, str(generator), harness, str(Path(directory) / shape),
                    "--shape", shape, "--records", "3", "--payload-bytes", "257", "--escaped"], text=True))
                raw = Path(manifest["path"]).read_text()
                if harness == "codex":
                    payload = 0
                    for row in map(json.loads, raw.splitlines()):
                        body = row["payload"]
                        if "type" not in body:
                            continue
                        if body["type"] in ("user_message", "agent_message"):
                            payload += len(body["message"])
                        elif body["type"] == "message":
                            payload += len(body["content"][0]["text"])
                        elif body["type"] == "reasoning":
                            payload += len(body["content"][0]["text"])
                        elif body["type"] == "function_call" and body["name"] == "Read":
                            payload += len(json.loads(body["arguments"])["payload"])
                else:
                    payload = sum(len(row["thoughts"][0]["description"])
                                  + len(row["toolCalls"][0]["result"][1]["text"])
                                  + len(row["toolCalls"][0]["result"][2]["functionResponse"]["response"]["output"])
                                  + len(row["toolCalls"][1]["result"]["z"])
                                  for row in ([json.loads(line) for line in raw.splitlines()] if shape == "selected-jsonl"
                                              else json.loads(raw)["messages"]) if "thoughts" in row)
                self.assertEqual(payload, 257)

    def test_selected_fixture_native_shapes_and_late_sentinel(self):
        generator = Path(__file__).with_name("perf-fixture.py")
        with tempfile.TemporaryDirectory(prefix="cv-test-selected-") as directory:
            for harness, shape in budget.SELECTED_PAYLOAD_CASES:
                command = [sys.executable, str(generator), "cursor" if harness == "cursor-ide" else harness,
                           str(Path(directory) / f"{harness}-{shape}"), "--shape", shape,
                           "--records", "2", "--payload-bytes", "257", "--escaped"]
                manifest = json.loads(subprocess.check_output(command, text=True))
                self.assertEqual(manifest["decoded_payload_bytes"], 257)
                self.assertEqual(manifest["selected_payload_bytes"], 257)
                self.assertGreater(manifest["physical_input_bytes"], 257)
                self.assertGreaterEqual(manifest["expected_messages"], 3)
                root = Path(manifest["root"])
                if Path(manifest["path"]).is_file() and harness not in ("kimi-code", "opencode-files", "codex", "gemini"):
                    with sqlite3.connect(manifest["path"]) as db:
                        tables = [name for name, in db.execute("SELECT name FROM sqlite_master WHERE type='table'")]
                        sentinel = any(b"gate-final" in str(value).encode() for table in tables
                                       for row in db.execute(f'SELECT * FROM "{table}"') for value in row)
                else:
                    sentinel = any(b"gate-final" in path.read_bytes() for path in root.rglob("*") if path.is_file())
                self.assertTrue(sentinel, "fixture must contain the late tool sentinel")

    def sqlite_driver(self):
        # Import the actual command under Python -I without relying on ambient module paths.
        tools = Path(__file__).parent
        schema_spec = importlib.util.spec_from_file_location("perf_fixture_sqlite", tools / "perf_fixture_sqlite.py")
        schema = importlib.util.module_from_spec(schema_spec)
        schema_spec.loader.exec_module(schema)
        driver_spec = importlib.util.spec_from_file_location("sqlite_budget", tools / "perf-sqlite-check.py")
        driver = importlib.util.module_from_spec(driver_spec)
        with patch.dict(sys.modules, {"perf_fixture_sqlite": schema}):
            driver_spec.loader.exec_module(driver)
        return driver

    def test_sqlite_clean_binary_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "probe-heap"):
            self.sqlite_driver().checked_peak({"build_features": {"mmap": True, "probe_heap": False}})

    def test_sqlite_missing_counter_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "heap counters"):
            self.sqlite_driver().checked_peak({"build_features": {"mmap": True, "probe_heap": True}})


if __name__ == "__main__":
    unittest.main()
