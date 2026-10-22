#!/usr/bin/env python3
"""Tests for the shared corpus tools, with no network: key_proxy.py against a local mock upstream
(every translator, and every generator's own `corpus_proxy` line), and scrub.py over a synthetic
jsonl, json and sqlite store.

    python3 -I tools/harness-corpus/test_tools.py
"""
import glob
import importlib.util
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from socketserver import TCPServer

HERE = os.path.dirname(os.path.abspath(__file__))
# A subprocess guard that only stops a hung driver; a busy machine must not fail a passing run.
HANG = 120
KEY = "sk-corpustest0123456789abcdefXYZ"


class LocalServer(ThreadingHTTPServer):
    def server_bind(self):
        # The mock is entirely local and must not wait for the machine's DNS resolver.
        TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


class Upstream(BaseHTTPRequestHandler):
    """A chat-completions and Anthropic-messages model that records what reached it."""
    seen = []

    def log_message(self, *a):
        pass

    def answer(self, code, obj, ctype="application/json"):
        b = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        Upstream.seen.append({"method": "GET", "path": self.path, "headers": dict(self.headers), "body": None})
        self.answer(200, {"object": "list", "data": [{"id": "upstream-model"}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        Upstream.seen.append({"method": "POST", "path": self.path, "headers": dict(self.headers), "body": body})
        if self.path.endswith("/messages"):
            return self.answer(200, {"type": "message", "role": "assistant", "content": [{"type": "text", "text": "hi"}]})
        msg = {"role": "assistant", "content": "hello", "reasoning_content": "thinking",
               "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\": \"ls\"}"}}]}
        if body.get("stream"):
            chunk = {"choices": [{"delta": {"content": "hello"}}]}
            return self.answer(200, f"data: {json.dumps(chunk)}\n\ndata: [DONE]\n\n".encode(), "text/event-stream")
        self.answer(200, {"choices": [{"message": msg}], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})


def post(url, obj, headers=None):
    req = urllib.request.Request(url, data=json.dumps(obj).encode(), method="POST",
                                 headers={"Content-Type": "application/json", "Authorization": "Bearer dummy", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def get(url):
    with urllib.request.urlopen(url, timeout=10) as r:
        return r.status, r.read()


# One request per translator, in the harness's own dialect: (path, body, check(status, bytes)).
def chat_request(model="m"):
    return "/v1/chat/completions", {"model": model, "messages": [
        {"role": "developer", "content": "be brief"},
        {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {"url": "data:x"}}]}],
        "tools": [{"type": "function", "function": {"name": "shell", "parameters": {}}}], "store": True}


DIALECT = {
    "passthrough": lambda: ("/v1/chat/completions", {"model": "m", "messages": [{"role": "user", "content": "hi"}],
                                                     "tools": [{"type": "function", "function": {"name": "shell"}}]}),
    "chat": chat_request,
    "responses": lambda: ("/v1/responses", {"model": "gpt-x", "instructions": "be brief", "input": [
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}]}),
    "gemini": lambda: ("/v1beta/models/gemini-2.5-flash:generateContent", {
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "tools": [{"functionDeclarations": [{"name": "shell", "parameters": {"type": "OBJECT", "properties": {}}}]}]}),
}


class ProxyCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.up = LocalServer(("127.0.0.1", 0), Upstream)
        threading.Thread(target=cls.up.serve_forever, daemon=True).start()
        cls.upstream = f"http://127.0.0.1:{cls.up.server_address[1]}/api/v4"
        cls.tmp = tempfile.TemporaryDirectory(prefix="cv-corpus-test.")
        cls.keyfile = os.path.join(cls.tmp.name, "key")
        with open(cls.keyfile, "w") as f:
            f.write(KEY + "\n")

    @classmethod
    def tearDownClass(cls):
        cls.up.shutdown()
        cls.tmp.cleanup()

    def setUp(self):
        Upstream.seen.clear()

    def start(self, translator, *args, max_calls=5):
        d = tempfile.mkdtemp(dir=self.tmp.name)
        log, portf = os.path.join(d, "calls.jsonl"), os.path.join(d, "port")
        cmd = [sys.executable, "-I", os.path.join(HERE, "key_proxy.py"), "--translate", translator,
               "--upstream", self.upstream, "--key-file", self.keyfile, "--log", log, "--port-file", portf,
               "--max-calls", str(max_calls), *args]
        p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: (p.kill(), p.wait(), p.stdout.close(), p.stderr.close()))
        for _ in range(200):
            if os.path.exists(portf):
                break
            time.sleep(0.02)
        with open(portf) as f:
            port = int(f.read())
        self.assertNotIn(KEY, " ".join(cmd))
        return f"http://127.0.0.1:{port}", log

    def calls(self, log):
        with open(log) as f:
            text = f.read()
        self.assertNotIn(KEY, text)
        return [json.loads(l) for l in text.splitlines()]

    def assert_keyed(self, seen, header="Authorization"):
        want = KEY if header == "x-api-key" else "Bearer " + KEY
        self.assertEqual({k.lower(): v for k, v in seen["headers"].items()}.get(header.lower()), want)

    def test_every_translator_passes_one_request_with_the_key(self):
        for tr in DIALECT:
            with self.subTest(tr):
                Upstream.seen.clear()
                url, log = self.start(tr, "--model", "glm-test")
                path, body = DIALECT[tr]()
                status, out = post(url + path, body)
                self.assertEqual(status, 200, out)
                self.assertNotIn(KEY.encode(), out)
                self.assertEqual(len(Upstream.seen), 1)
                self.assertEqual(Upstream.seen[0]["path"], "/api/v4/chat/completions")
                self.assert_keyed(Upstream.seen[0])
                self.assertEqual(Upstream.seen[0]["body"]["model"], "glm-test")
                [line] = self.calls(log)
                self.assertFalse(line["refused"])
                self.assertEqual(line["tools"], ["shell"])
                if tr == "responses":
                    self.assertIn(b"event: response.completed", out)
                    self.assertIn(b'"type": "function_call"', out)
                if tr == "gemini":
                    parts = json.loads(out)["candidates"][0]["content"]["parts"]
                    self.assertEqual(parts[-1]["functionCall"], {"name": "shell", "args": {"cmd": "ls"}, "id": "call_1"})
                    self.assertEqual(Upstream.seen[0]["body"]["tools"][0]["function"]["parameters"]["type"], "object")

    def test_chat_adapts_the_request_for_glm(self):
        url, _ = self.start("chat", "--model", "glm-test", "--also", "glm-alt")
        path, body = chat_request()
        post(url + path, body)
        sent = Upstream.seen[-1]["body"]
        self.assertEqual([m["role"] for m in sent["messages"]], ["system", "user"])
        self.assertIn("cannot see images", sent["messages"][1]["content"][1]["text"])
        self.assertNotIn("store", sent)
        self.assertEqual(sent["thinking"], {"type": "enabled"})
        post(url + path, chat_request("glm-alt")[1])
        self.assertEqual(Upstream.seen[-1]["body"]["model"], "glm-alt")
        status, out = get(url + "/v1/models")
        self.assertEqual([m["id"] for m in json.loads(out)["data"]], ["glm-test", "glm-alt"])
        self.assertEqual(len(Upstream.seen), 2)

    def test_chat_streams_the_upstream_bytes_back(self):
        url, _ = self.start("chat", "--model", "glm-test", "--thinking", "harness")
        path, body = chat_request()
        status, out = post(url + path, {**body, "stream": True})
        self.assertEqual(status, 200)
        self.assertTrue(out.endswith(b"data: [DONE]\n\n"))
        self.assertNotIn("thinking", Upstream.seen[-1]["body"])

    def test_injected_provider_errors_are_not_forwarded_or_counted(self):
        url, log = self.start("chat", "--model", "glm-test", max_calls=1)
        self.assertEqual(post(url + "/v1/chat/completions", {"model": "corpus-error", "messages": []})[0], 400)
        marker = {"model": "m", "messages": [{"role": "user", "content": "CORPUS-PROVIDER-ERROR: say ok"}]}
        self.assertEqual(post(url + "/v1/chat/completions", marker)[0], 500)
        self.assertEqual(Upstream.seen, [])
        self.assertEqual(post(url + "/v1/chat/completions", chat_request()[1])[0], 200)
        self.assertEqual([l.get("injected", False) for l in self.calls(log)], [True, True, False])
        # The same two scenarios on the passthrough wire (Claude Code's Anthropic messages), answered
        # in that wire's error dialect, so a generator can record a provider failure and a retry.
        Upstream.seen.clear()
        url, log = self.start("passthrough", max_calls=1)
        status, out = post(url + "/v1/messages", {"model": "m", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "CORPUS-PROVIDER-ERROR then say ok"}]}]})
        self.assertEqual(status, 500)
        # A resumed session carries the failed prompt and the new one in one user message: no injection.
        resumed = {"model": "m", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "CORPUS-PROVIDER-ERROR then say ok"}, {"type": "text", "text": "say recovered"}]}]}
        self.assertEqual(post(url + "/v1/messages", resumed)[0], 200)
        self.assertEqual(json.loads(out), {"type": "error", "error": {"type": "api_error", "message": "corpus: injected provider error"}})
        self.assertEqual(post(url + "/v1/messages", {"model": "corpus-error", "messages": []})[0], 400)
        self.assertEqual([s["body"]["messages"][0]["content"][-1]["text"] for s in Upstream.seen], ["say recovered"])
        self.assertEqual([l.get("injected", False) for l in self.calls(log)], [True, False, True])

    def test_calls_past_the_cap_get_a_400_and_never_reach_upstream(self):
        for tr in DIALECT:
            with self.subTest(tr):
                Upstream.seen.clear()
                url, log = self.start(tr, "--model", "glm-test", max_calls=1)
                path, body = DIALECT[tr]()
                self.assertEqual(post(url + path, body)[0], 200)
                status, out = post(url + path, body)
                self.assertEqual(status, 400)
                self.assertIn(b"corpus budget", out)
                self.assertEqual(len(Upstream.seen), 1)
                self.assertEqual([l["refused"] for l in self.calls(log)], [False, True])

    def test_passthrough_keeps_the_path_and_can_use_x_api_key(self):
        url, log = self.start("passthrough", "--auth", "x-api-key")
        status, _ = post(url + "/v1/messages", {"model": "claude-x", "messages": [{"role": "user", "content": "hi"}]})
        self.assertEqual(status, 200)
        self.assertEqual(Upstream.seen[0]["path"], "/api/v4/messages")
        self.assert_keyed(Upstream.seen[0], "x-api-key")
        self.assertEqual(Upstream.seen[0]["body"]["model"], "claude-x")
        get(url + "/v1/models")  # a model list passes through uncounted
        self.assertEqual(Upstream.seen[1]["path"], "/api/v4/models")
        self.assertEqual(len(self.calls(log)), 1)

    def test_every_generator_starts_its_proxy_and_passes_a_request(self):
        """Each generator's own `corpus_proxy` line, through lib.sh, with the upstream swapped for the mock."""
        lines = []
        for gen in sorted(glob.glob(os.path.join(HERE, "*", "*.sh"))):
            with open(gen) as f:
                lines += [(os.path.relpath(gen, HERE), l.strip()) for l in f if l.startswith("corpus_proxy ")]
        self.assertGreaterEqual(len(lines), 14)
        for gen, line in lines:
            with self.subTest(gen):
                Upstream.seen.clear()
                tr = line.split()[2]
                line = re.sub(r'"https://[^"]+"', '"$UPSTREAM"', line)
                work = tempfile.mkdtemp(dir=self.tmp.name)
                script = (f'set -euo pipefail; . "{HERE}/lib.sh"; WORK="{work}"; UPSTREAM="{self.upstream}"; '
                          f'ZAI_KEY_FILE="{self.keyfile}"; KIMI_KEY_FILE="{self.keyfile}"; '
                          'MODEL=glm-test SWITCH=glm-alt SWITCH_MODEL=glm-alt; '
                          f'{line}; echo "$PORT"; read -r _; corpus_proxy_stop')
                b = subprocess.Popen(["bash", "-c", script], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
                try:
                    port = int(b.stdout.readline())
                    path, body = DIALECT[tr]()
                    status, out = post(f"http://127.0.0.1:{port}{path}", body)
                    self.assertEqual(status, 200, out)
                    self.assertEqual(len(Upstream.seen), 1)
                    self.assertTrue(any(KEY in v for v in Upstream.seen[0]["headers"].values()))
                finally:
                    b.communicate("\n", timeout=10)
                self.assertEqual(b.returncode, 0)


class ScrubCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="cv-corpus-test.")
        # As in a generator: the store is copied out of the throwaway dir before it is scrubbed.
        self.work = os.path.join(os.path.realpath(self.tmp.name), "cv-corpus.test.000000")
        self.home = os.path.join(self.work, "home")
        self.out = os.path.join(os.path.realpath(self.tmp.name), "out")
        os.makedirs(self.work)
        os.makedirs(self.out)
        self.real_home = "/Users/zorblax"
        self.keyfile = os.path.join(self.work, "key")
        with open(self.keyfile, "w") as f:
            f.write("literal-secret-value-1234\n")

    def tearDown(self):
        self.tmp.cleanup()

    def scrub(self):
        env = {**os.environ, "CORPUS_REAL_HOME": self.real_home, "CORPUS_KEY_FILES": self.keyfile}
        return subprocess.run([sys.executable, "-I", os.path.join(HERE, "scrub.py"), "--work", self.work,
                               "--home", self.home, "--dir", self.out], env=env, capture_output=True, text=True)

    def write_store(self, extra=""):
        proj = self.home + "/project"
        dashed = re.sub(r"[^A-Za-z0-9]", "-", proj)
        os.makedirs(os.path.join(self.out, "projects", dashed))
        with open(os.path.join(self.out, "projects", dashed, "s.jsonl"), "w") as f:
            f.write(json.dumps({"cwd": proj, "tmp": self.work + "/tmp/x", "text": "ls -l: zorblax staff " + extra}) + "\n")
            f.write(json.dumps({"email": "zorblax@real.example.com", "ok": "a@x.invalid"}) + "\n")
        # An encoder that escapes `/` writes the path as `\/home\/...`.
        with open(os.path.join(self.out, "index.json"), "w") as f:
            f.write(json.dumps({"tool": self.real_home + "/tools/harness-versions/h/1/bin/h", "escaped": proj,
                                "account_id": "acct-123"}).replace("/", "\\/"))
        con = sqlite3.connect(os.path.join(self.out, "state.db"))
        con.execute("PRAGMA journal_mode=WAL")
        con.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, txt TEXT, blob BLOB)")
        con.execute("INSERT INTO t (txt, blob) VALUES (?, ?)", (proj + "/a.txt", (self.real_home + "/notes").encode()))
        con.commit()
        con.close()

    def test_placeholders_replace_paths_users_and_accounts(self):
        self.write_store()
        r = self.scrub()
        self.assertEqual(r.returncode, 0, r.stderr)
        [d] = os.listdir(os.path.join(self.out, "projects"))
        self.assertEqual(d, "-home-corpus-project")
        with open(os.path.join(self.out, "projects", d, "s.jsonl")) as f:
            rows = [json.loads(l) for l in f]
        self.assertEqual(rows[0]["cwd"], "/home/corpus/project")
        self.assertEqual(rows[0]["tmp"], "/home/corpus/.work/tmp/x")
        self.assertIn("corpus staff", rows[0]["text"])
        self.assertEqual(rows[1], {"email": "corpus@example.invalid", "ok": "a@x.invalid"})
        with open(os.path.join(self.out, "index.json")) as f:
            raw = f.read()
        idx = json.loads(raw)
        self.assertIn(r'"escaped": "\/home\/corpus\/project"', raw)
        self.assertEqual(idx["tool"], "/opt/harness-versions/h/1/bin/h")
        self.assertEqual(idx["account_id"], "corpus@example.invalid")
        con = sqlite3.connect(os.path.join(self.out, "state.db"))
        self.assertEqual(con.execute("SELECT txt, blob FROM t").fetchone(), ("/home/corpus/project/a.txt", b"/home/corpus/notes"))
        self.assertEqual(con.execute("PRAGMA journal_mode").fetchone()[0], "delete")
        con.close()

    def test_secrets_are_refused_by_file_never_by_value(self):
        for secret in ("literal-secret-value-1234", "sk-" + "a" * 24, "ghp_" + "b" * 36, "Bearer " + "c" * 24,
                       "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.sig", "0123456789abcdef0123456789abcdef.ABCDEFGHijklmnop",
                       "AKIA" + "Q" * 16):
            with self.subTest(secret[:8]):
                shutil.rmtree(self.out)
                os.makedirs(self.out)
                self.write_store(secret)
                r = self.scrub()
                self.assertEqual(r.returncode, 1)
                self.assertIn("s.jsonl", r.stderr)
                self.assertNotIn(secret, r.stderr + r.stdout)


class TuiTests(unittest.TestCase):
    def test_hermes_driver_reports_early_exit_and_natural_failure(self):
        with tempfile.TemporaryDirectory(prefix="cv-hermes-drive-") as directory:
            for child, expected in [("pass", 3), ('import sys; print("ready",flush=True); sys.stdin.readline(); sys.exit(7)', 7)]:
                with self.subTest(expected=expected):
                    result = subprocess.run([sys.executable, "-I", os.path.join(HERE, "hermes", "drive_tty.py"),
                                             "--log", os.path.join(directory, "output.log"), "--idle", "0.01", "--startup", "0.01",
                                             "--min", "0.01", "--timeout", "5", "-l", "hello", "--", sys.executable, "-c", child],
                                            capture_output=True, text=True, timeout=HANG)
                    self.assertEqual(result.returncode, expected, result.stderr)

    def test_early_exit_does_not_satisfy_required_expect(self):
        with tempfile.TemporaryDirectory(prefix="cv-tui-control-") as directory:
            script = os.path.join(directory, "steps.json")
            log = os.path.join(directory, "output.log")
            for steps in ([{"expect": "required-screen", "timeout": 0.1}],
                          [{"sleep": 0.1}, {"expect": "required-screen"}],
                          [{"expect": "optional", "optional": True}, {"expect": "required-screen"}]):
                with self.subTest(steps=steps):
                    with open(script, "w") as file:
                        json.dump(steps, file)
                    result = subprocess.run([sys.executable, "-I", os.path.join(HERE, "tui_driver.py"), log,
                                             sys.executable, "-c", "pass"], env=dict(os.environ, TUI_SCRIPT=script),
                                            capture_output=True, text=True, timeout=HANG)
                    self.assertEqual(result.returncode, 3, result.stderr)
                    with open(log) as file:
                        self.assertIn("child exited before", file.read())

    def test_natural_child_failure_is_not_reported_as_success(self):
        with tempfile.TemporaryDirectory(prefix="cv-tui-status-") as directory:
            script = os.path.join(directory, "steps.json")
            with open(script, "w") as file:
                json.dump([], file)
            result = subprocess.run([sys.executable, "-I", os.path.join(HERE, "tui_driver.py"),
                                     os.path.join(directory, "output.log"), sys.executable, "-c", "import sys; sys.exit(7)"],
                                    env=dict(os.environ, TUI_SCRIPT=script), capture_output=True, text=True, timeout=HANG)
            self.assertEqual(result.returncode, 7, result.stderr)

    def test_sigterm_ignoring_owned_child_is_reaped(self):
        with tempfile.TemporaryDirectory(prefix="cv-tui-stop-") as directory:
            script = os.path.join(directory, "steps.json")
            with open(script, "w") as file:
                json.dump([{"expect": "ready"}, {"expect": "never", "timeout": 0.02}], file)
            child = 'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print("ready",flush=True); time.sleep(60)'
            result = subprocess.run([sys.executable, "-I", os.path.join(HERE, "tui_driver.py"),
                                     os.path.join(directory, "output.log"), sys.executable, "-c", child],
                                    env=dict(os.environ, TUI_SCRIPT=script), capture_output=True, text=True, timeout=HANG)
            self.assertEqual(result.returncode, 3, result.stderr)


class SnapshotTests(unittest.TestCase):
    def test_existing_versions_are_refused_before_opening_stores(self):
        with tempfile.TemporaryDirectory(prefix="cv-snapshot-refusal-") as directory:
            for harness in ("zed", "lmstudio"):
                with self.subTest(harness=harness):
                    root = os.path.join(directory, harness)
                    tools = os.path.join(root, "tools", "harness-corpus", harness)
                    os.makedirs(tools)
                    script = os.path.join(tools, "snapshot.sh")
                    shutil.copyfile(os.path.join(HERE, harness, "snapshot.sh"), script)
                    version = os.path.join(root, "crates", "cv-core", "tests", "corpus", harness, "fixture-version")
                    os.makedirs(version)
                    sentinel = os.path.join(version, "preserve.txt")
                    with open(sentinel, "w") as file:
                        file.write("original")
                    commands = os.path.join(root, "bin")
                    os.mkdir(commands)
                    defaults = os.path.join(commands, "defaults")
                    with open(defaults, "w") as file:
                        file.write("#!/bin/sh\necho fixture-version\n")
                    os.chmod(defaults, 0o755)
                    result = subprocess.run(["bash", script], capture_output=True, text=True,
                                            env=dict(os.environ, PATH=commands + os.pathsep + os.environ["PATH"]))
                    self.assertEqual(result.returncode, 1)
                    self.assertIn("instead of regenerating", result.stderr)
                    with open(sentinel) as file:
                        self.assertEqual(file.read(), "original")
                    unknown = subprocess.run(["bash", script, "--typo"], capture_output=True, text=True)
                    self.assertEqual(unknown.returncode, 2)


class GuardTests(unittest.TestCase):
    def test_guard_refuses_outside_and_symlink_paths_without_creating_them(self):
        with tempfile.TemporaryDirectory(prefix="cv-guard-control-") as root:
            work = os.path.realpath(os.path.join(root, "work"))
            outside = os.path.join(root, "outside")
            os.mkdir(work)
            os.mkdir(outside)
            os.symlink(outside, os.path.join(work, "link"))
            for path in (os.path.join(root, "refused"), os.path.join(work, "link", "refused")):
                with self.subTest(path=path):
                    result = subprocess.run(["bash", "-c", 'source "$1"; WORK="$2"; CHECK_DIR="$3"; corpus_guard CHECK_DIR',
                                             "guard", os.path.join(HERE, "lib.sh"), work, path], capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("outside the throwaway", result.stderr)
                    self.assertFalse(os.path.exists(path), "refusal created the forbidden directory")
            allowed = os.path.join(work, "allowed", "nested")
            result = subprocess.run(["bash", "-c", 'source "$1"; WORK="$2"; CHECK_DIR="$3"; corpus_guard CHECK_DIR',
                                     "guard", os.path.join(HERE, "lib.sh"), work, allowed], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(os.path.isdir(allowed))


schema_test_spec = importlib.util.spec_from_file_location("schema_snapshot_tests", os.path.join(HERE, "test_schema_snapshot.py"))
schema_tests = importlib.util.module_from_spec(schema_test_spec)
schema_test_spec.loader.exec_module(schema_tests)
SchemaSnapshotCase = schema_tests.SchemaSnapshotCase


if __name__ == "__main__":
    unittest.main(verbosity=1)
