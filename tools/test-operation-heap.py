#!/usr/bin/env python3
"""Budget enforcement controls; missing measurements must never pass CI."""
import importlib.util
import hashlib
import json
import tempfile
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("operation_heap", Path(__file__).with_name("check-operation-heap.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class BudgetTests(unittest.TestCase):
    @staticmethod
    def measured(peak):
        return dict(peak_above_baseline_bytes=peak, allocation_requests=100, requested_bytes=1 << 20)

    def test_rejects_owned_same_gemini_rehome(self):
        with self.assertRaises(AssertionError):
            gate.checked_peak(dict(peak_above_baseline_bytes=67118262,
                                   allocation_requests=1834, requested_bytes=67781518))

    def test_missing_or_invalid_counters_fail(self):
        for value in (None, {}, {"peak_above_baseline_bytes": None},
                      {"peak_above_baseline_bytes": True}, {"peak_above_baseline_bytes": -1},
                      self.measured(0), {"peak_above_baseline_bytes": 463595}):
            with self.subTest(value=value), self.assertRaises(AssertionError):
                gate.checked_peak(value)

    def test_inactive_allocator_or_missing_request_counters_fail(self):
        for name in ("allocation_requests", "requested_bytes"):
            for value in (None, True, 0, -1):
                heap = self.measured(463595)
                heap[name] = value
                with self.subTest(name=name, value=value), self.assertRaises(AssertionError):
                    gate.checked_peak(heap)

    def test_old_whole_payload_ownership_exceeds_cap(self):
        with self.assertRaisesRegex(AssertionError, "exceeds"):
            gate.checked_peak(self.measured(201466250))

    def test_previous_codex_thinking_dispatch_exceeds_cap(self):
        for peak in (67401698, 67198981):
            with self.subTest(peak=peak), self.assertRaisesRegex(AssertionError, "exceeds"):
                gate.checked_peak(self.measured(peak))

    def test_codex_digest_normalizes_only_package_version(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output.jsonl"
            header = dict(type="session_meta", timestamp="fixed", payload=dict(cli_version="0.16.1", id="fixed-id"))
            body = json.dumps(dict(type="response_item", payload=dict(text="x" * 150000, cli_version="nested-kept"))).encode() + b"\n"
            normalized = dict(header, payload=dict(header["payload"], cli_version="VERSION"))
            expected = hashlib.sha256(json.dumps(normalized, ensure_ascii=False, separators=(",", ":")).encode() + b"\n" + body).hexdigest()
            def write():
                path.write_bytes(json.dumps(header).encode() + b"\n" + body)
            write()
            self.assertEqual(gate.codex_output_digest(path), (expected, 2))
            header["payload"]["cli_version"] = "future-version"
            write()
            self.assertEqual(gate.codex_output_digest(path), (expected, 2))
            body = body.replace(b"nested-kept", b"nested-lost")
            write()
            self.assertNotEqual(gate.codex_output_digest(path)[0], expected)
            path.write_bytes(path.read_bytes()[:-1])
            with self.assertRaises(AssertionError):
                gate.codex_output_digest(path)
            path.write_bytes(b'{}\n')
            with self.assertRaises((AssertionError, KeyError)):
                gate.codex_output_digest(path)

    def test_previous_same_harness_complete_ownership_exceeds_cap(self):
        with self.assertRaisesRegex(AssertionError, "exceeds"):
            gate.checked_peak(self.measured(267060266))

    def test_measured_peak_with_platform_slack_passes(self):
        self.assertEqual(gate.checked_peak(self.measured(463595)), 463595)


    def test_export_digest_normalizes_only_linked_top_level_ids(self):
        first = "11111111-1111-4111-8111-111111111111"
        second = "22222222-2222-4222-8222-222222222222"
        body = 'λ\n' * 40000 + '"uuid":"' + first + '"'
        rows = [dict(uuid=first, parentUuid=None, content=body, nested=dict(uuid=first)),
                dict(uuid=second, parentUuid=first, content="kept result")]
        expected = [dict(rows[0], uuid="generated-0"),
                    dict(rows[1], uuid="generated-1", parentUuid="generated-0")]
        expected_hash = hashlib.sha256(json.dumps(expected, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output.jsonl"
            def write():
                path.write_text("".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n" for row in rows))
            write()
            self.assertEqual(gate.claude_output_digest(path), (expected_hash, 2))
            rows[0]["uuid"] = "33333333-3333-4333-8333-333333333333"
            rows[1]["parentUuid"] = rows[0]["uuid"]
            write()
            self.assertEqual(gate.claude_output_digest(path), (expected_hash, 2))
            rows[1]["content"] = "silently lost result"
            write()
            self.assertNotEqual(gate.claude_output_digest(path)[0], expected_hash)
            rows[1]["content"] = "kept result"
            rows[0]["nested"]["uuid"] = rows[0]["uuid"]
            write()
            self.assertNotEqual(gate.claude_output_digest(path)[0], expected_hash)

    def test_export_digest_rejects_unexpected_header_and_incomplete_output(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output.jsonl"
            for wire in (b'{"content":"unexpected field order"}\n',
                         b'{"uuid":"11111111-1111-4111-8111-111111111111","parentUuid":null,"content":"missing newline"}'):
                path.write_bytes(wire)
                with self.subTest(wire=wire), self.assertRaises(AssertionError):
                    gate.claude_output_digest(path)



    def test_gemini_digest_keeps_tool_nested_ids_and_all_payload_bytes(self):
        identifier = "11111111-1111-5111-8111-111111111111"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output.json"
            header = b'      "id": "' + identifier.encode() + b'",\n'
            payload = b'        "text": "' + b'x' * 150000 + b'",\n'
            nested = b'            "id": "' + identifier.encode() + b'",\n'
            path.write_bytes(header + payload + nested)
            normalized = header.replace(identifier.encode(), b"00000000-0000-4000-8000-000000000000")
            expected = hashlib.sha256(normalized + payload + nested).hexdigest()
            self.assertEqual(gate.gemini_output_digest(path), (expected, 1))
            path.write_bytes(header + payload + nested.replace(identifier.encode(), b"22222222-2222-5222-8222-222222222222"))
            self.assertNotEqual(gate.gemini_output_digest(path)[0], expected)
            path.write_bytes(header + payload.replace(b'x', b'y') + nested)
            self.assertNotEqual(gate.gemini_output_digest(path)[0], expected)
            path.write_bytes(header.replace(b"5111", b"4111") + payload + nested)
            self.assertEqual(gate.gemini_output_digest(path)[1], 0)


if __name__ == "__main__":
    unittest.main()
