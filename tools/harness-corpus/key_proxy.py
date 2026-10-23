#!/usr/bin/env python3
"""The one local model endpoint every corpus generator points its harness at. It alone holds the key.

The harness is given http://127.0.0.1:PORT and a dummy key. Each model call is forwarded to
--upstream with the real key, translated when the harness speaks a different wire:

  passthrough  a wire the upstream already speaks (chat completions, Anthropic messages). Bytes go
               up as sent (--model rewrites "model") and stream back as they arrive. The path after
               the proxy root is appended to --upstream, minus a leading /vN when the upstream
               already ends in one, so a harness may be given either `.../v1` or the bare root.
  chat         OpenAI chat completions, adapted for Z.ai GLM: "model" pinned to --model (an --also
               model passes through, for a real model switch), thinking on unless the harness set it
               (--thinking harness leaves it alone), `developer` becomes `system`, image parts become
               a text placeholder (the model is text-only; the harness still persists the image),
               OpenAI-only knobs Z.ai rejects are dropped. GET .../models answers locally.
  responses    the OpenAI Responses API (Codex), answered from chat completions as SSE.
  gemini       the Gemini API (generateContent, streamGenerateContent, countTokens answered locally),
               answered from chat completions.

    key_proxy.py --translate chat --upstream https://api.z.ai/api/coding/paas/v4 --key-file ~/.zai-key \
        --log calls.jsonl --port-file port [--max-calls 40] [--model glm-5.3-flash] [--also glm-4.5-air]

The key is the first line of --key-file. It never enters argv, the environment, a log or a reply.
--port 0 (the default) binds a free port; --port-file receives it once the proxy is listening.

Every model call is one JSON line in --log: the path, the model the harness asked for, the stream
flag, the request's shape (message roles, or Responses item / Gemini part kinds), the offered tool
NAMES, and "refused". Never content or headers. Past --max-calls a call is answered here with a 400
in the harness's own error dialect (a 400, not a 429, so no harness sits retrying) and never sent.
Two provider-error scenarios are answered here too, uncounted and logged as {"injected": true}, on
the chat and passthrough wires (each in its own error dialect): a request for model `corpus-error`
gets a 400, and one whose last user message contains CORPUS-PROVIDER-ERROR gets a 500.
"""
import argparse
import json
import os
import re
import sys
import threading
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from socketserver import TCPServer

FAIL_MARKER = "CORPUS-PROVIDER-ERROR"
FAIL_MODEL = "corpus-error"
IMAGE_PLACEHOLDER = "[image attached; this model cannot see images]"
IMAGE_PARTS = ("image_url", "input_image", "image", "file")
ZAI_REJECTS = ("store", "prompt_cache_key", "reasoning_effort", "parallel_tool_calls", "metadata", "service_tier")
HOP = {"host", "authorization", "x-api-key", "content-length", "connection", "accept-encoding",
       "transfer-encoding", "keep-alive", "proxy-connection"}

ARGS = None
KEY = None
LOCK = threading.Lock()
CALLS = 0


# --- budget and log ---------------------------------------------------------------------------

def injection(req):
    """The two provider-error scenarios, for every messages-shaped wire: `(status, message)` to answer
    locally (logged as injected, never counted, never forwarded), else None."""
    if req.get("model") == FAIL_MODEL:
        status, message = 400, "corpus: this model always fails"
    else:
        # The marker must be in the LAST text of the last user message: a harness that resumes a failed
        # session sends its old prompt and the new one as one user message, and only the new one counts.
        users = [m for m in req.get("messages") or [] if isinstance(m, dict) and m.get("role") == "user"]
        content = users[-1].get("content") if users else None
        if isinstance(content, list):
            texts = [p.get("text") for p in content if isinstance(p, dict) and isinstance(p.get("text"), str)]
            content = texts[-1] if texts else ""
        if not (isinstance(content, str) and FAIL_MARKER in content):
            return None
        status, message = 500, "corpus: injected provider error"
    log_line({"injected": True, "status": status, "stream": bool(req.get("stream"))})
    return status, message


def log_line(entry):
    with LOCK, open(ARGS.log, "a") as f:
        f.write(json.dumps(entry) + "\n")


def admit(path, asked, stream, shape, tools, **extra):
    """Count one model call and log its shape; False when it is past --max-calls."""
    global CALLS
    with LOCK:
        CALLS += 1
        call = CALLS
        refused = bool(ARGS.max_calls) and call > ARGS.max_calls
        with open(ARGS.log, "a") as f:
            f.write(json.dumps({"call": call, "path": path, "model_asked": asked, "stream": bool(stream),
                                "shape": shape, "tools": tools, **extra, "refused": refused}) + "\n")
    return not refused


def budget_message():
    return f"corpus budget of {ARGS.max_calls} model calls spent (key_proxy --max-calls)"


# --- upstream ---------------------------------------------------------------------------------

def auth_headers():
    if ARGS.auth == "x-api-key":
        return {"x-api-key": KEY}
    return {"Authorization": "Bearer " + KEY}


def open_upstream(url, body, headers, method="POST"):
    """The upstream response (an HTTPError is a response too)."""
    req = urllib.request.Request(url, data=body, method=method, headers={**headers, **auth_headers()})
    try:
        return urllib.request.urlopen(req, timeout=600)
    except urllib.error.HTTPError as e:
        return e


def chat_url():
    return ARGS.upstream.rstrip("/") + "/chat/completions"


def chat_call(body):
    """One non-streaming chat completion, as a dict; raises on an upstream error."""
    resp = open_upstream(chat_url(), json.dumps(body).encode(), {"Content-Type": "application/json"})
    with resp:
        status = resp.status if hasattr(resp, "status") else resp.code
        data = resp.read()
    if status >= 400:
        raise RuntimeError(f"HTTP {status}: {data[:200].decode('utf-8', 'replace')}")
    return json.loads(data)


def tool_names(tools):
    out = []
    for t in tools or []:
        if isinstance(t, dict):
            out.append((t.get("function") or {}).get("name") or t.get("name") or t.get("type") or "?")
    return out


def roles(messages):
    return [m.get("role") for m in messages or [] if isinstance(m, dict)]


# --- translators ------------------------------------------------------------------------------

class Passthrough:
    """Forward as sent, add the key, stream the answer back byte for byte."""

    def target(self, path):
        if re.search(r"/v\d+$", ARGS.upstream.rstrip("/")) and re.match(r"^/v\d+(/|$)", path):
            path = re.sub(r"^/v\d+", "", path, count=1)
        return ARGS.upstream.rstrip("/") + path

    def refuse(self, h):
        h.reply_json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": budget_message()}})

    def handle(self, h, method, raw):
        if method == "POST" and raw:
            try:
                req = json.loads(raw)
            except ValueError:
                req = None
            if isinstance(req, dict) and "messages" in req:
                if hit := injection(req):
                    status, message = hit
                    kind = "invalid_request_error" if status == 400 else "api_error"
                    return h.reply_json(status, {"type": "error", "error": {"type": kind, "message": message}})
                if not admit(h.path.split("?")[0], req.get("model"), req.get("stream"), roles(req["messages"]),
                             tool_names(req.get("tools"))):
                    return self.refuse(h)
                if ARGS.model:
                    req["model"] = ARGS.model
                    raw = json.dumps(req).encode()
        headers = {k: v for k, v in h.headers.items() if k.lower() not in HOP}
        try:
            resp = open_upstream(self.target(h.path), raw or None, headers, method)
        except Exception as e:  # network: a gateway error, without the request
            return h.reply_json(502, {"error": {"message": f"key_proxy upstream: {type(e).__name__}"}})
        h.relay(resp)


class Chat(Passthrough):
    """OpenAI chat completions to an OpenAI-compatible upstream (Z.ai GLM)."""

    def models(self, h):
        h.reply_json(200, {"object": "list", "data": [
            {"id": m, "object": "model", "owned_by": "corpus", "context_length": ARGS.context}
            for m in [ARGS.model, *ARGS.also]]})

    def adapt(self, req):
        if req.get("model") not in ARGS.also:
            req["model"] = ARGS.model
        if ARGS.thinking == "on":
            req.setdefault("thinking", {"type": "enabled"})
        for k in ZAI_REJECTS:
            req.pop(k, None)
        for m in req.get("messages") or []:
            if m.get("role") == "developer":  # Z.ai: 400 "Incorrect role information"
                m["role"] = "system"
            c = m.get("content")
            if isinstance(c, list):
                m["content"] = [{"type": "text", "text": IMAGE_PLACEHOLDER}
                                if isinstance(p, dict) and p.get("type") in IMAGE_PARTS else p for p in c]
        return req

    def handle(self, h, method, raw):
        path = h.path.split("?")[0].rstrip("/")
        if method == "GET":
            return self.models(h) if path.endswith("/models") else h.reply_json(404, {"error": {"message": "not found"}})
        if not path.endswith("/chat/completions"):
            return h.reply_json(404, {"error": {"message": "only /chat/completions is served"}})
        req = json.loads(raw or b"{}")
        if hit := injection(req):
            status, message = hit
            kind = "invalid_request_error" if status == 400 else "server_error"
            return h.reply_json(status, {"error": {"message": message, "type": kind, "code": "corpus_error"}})
        if not admit(path, req.get("model"), req.get("stream"), roles(req.get("messages")), tool_names(req.get("tools"))):
            return self.refuse(h)
        body = json.dumps(self.adapt(req)).encode()
        try:
            resp = open_upstream(chat_url(), body, {"Content-Type": "application/json",
                                                    "Accept": h.headers.get("Accept", "*/*")})
        except Exception as e:  # never echo request headers
            return h.reply_json(502, {"error": {"message": "key_proxy upstream: " + str(e)[:200]}})
        h.relay(resp)


def text_of_items(content):
    if isinstance(content, str):
        return content
    out = []
    for p in content or []:
        t = p.get("type")
        if t in ("input_text", "output_text", "text", "summary_text", "reasoning_text"):
            out.append(p.get("text", ""))
        elif t == "input_image":
            out.append(IMAGE_PLACEHOLDER)
    return "\n".join(out)


class Responses:
    """The Responses API (Codex 0.16x, `wire_api = "responses"`) answered from chat completions."""

    @staticmethod
    def tools_to_chat(tools):
        """Responses tool list -> (chat tools, {flat name: (kind, namespace, name)})."""
        chat, names = [], {}

        def add(t, ns=None):
            kind, name = t.get("type"), t.get("name", "")
            flat = f"{ns}{name}" if ns else name
            if kind == "function":
                params = t.get("parameters") or {"type": "object", "properties": {}}
            elif kind == "custom":
                params = {"type": "object", "properties": {"input": {"type": "string", "description": "the raw tool input"}},
                          "required": ["input"]}
            else:
                return
            names[flat] = (kind, ns, name)
            chat.append({"type": "function", "function": {"name": flat, "description": (t.get("description") or "")[:2000],
                                                          "parameters": params}})

        for t in tools or []:
            if t.get("type") == "namespace":
                for c in t.get("tools", []):
                    add(c, t.get("name", ""))
            else:
                add(t)
        return chat, names

    @staticmethod
    def input_to_messages(instructions, items, names):
        msgs = [{"role": "system", "content": instructions}] if instructions else []
        rev = {(ns, n): flat for flat, (_, ns, n) in names.items()}

        def assistant():
            if not msgs or msgs[-1]["role"] != "assistant" or msgs[-1].get("_closed"):
                msgs.append({"role": "assistant", "content": ""})
            return msgs[-1]

        for it in items or []:
            t = it.get("type")
            if t == "message":
                role = {"developer": "system"}.get(it.get("role"), it.get("role"))
                txt = text_of_items(it.get("content"))
                if role == "assistant":
                    a = assistant()
                    a["content"] = (a["content"] + "\n" + txt).strip() if a["content"] else txt
                    a["_closed"] = True
                else:
                    msgs.append({"role": role, "content": txt})
            elif t in ("function_call", "custom_tool_call"):
                a = assistant()
                flat = rev.get((it.get("namespace"), it.get("name")), it.get("name"))
                args = it.get("arguments") if t == "function_call" else json.dumps({"input": it.get("input", "")})
                a.setdefault("tool_calls", []).append(
                    {"id": it.get("call_id"), "type": "function", "function": {"name": flat, "arguments": args or "{}"}})
                a.pop("_closed", None)
            elif t in ("function_call_output", "custom_tool_call_output"):
                out = it.get("output")
                msgs.append({"role": "tool", "tool_call_id": it.get("call_id"),
                             "content": out if isinstance(out, str) else text_of_items(out)})
            elif t in ("compaction", "context_compaction"):
                s = it.get("summary") or it.get("encrypted_content") or ""
                if isinstance(s, str) and s:
                    msgs.append({"role": "user", "content": s})
            # reasoning items are not forwarded (the model keeps no hidden state)
        for m in msgs:
            m.pop("_closed", None)
        return msgs

    def sse(self, h, evs):
        h.reply(200, "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in evs).encode(), "text/event-stream")

    def handle(self, h, method, raw):
        if method != "POST" or not h.path.split("?")[0].rstrip("/").endswith("/responses"):
            return h.reply_json(404, {"error": {"message": "only POST /responses is served"}})
        req = json.loads(raw or b"{}")
        # Responses-lite models get (some) tool definitions as `additional_tools` input items.
        all_tools = list(req.get("tools") or [])
        for it in req.get("input") or []:
            if it.get("type") == "additional_tools":
                all_tools += it.get("tools") or []
        if not admit(h.path, req.get("model"), req.get("stream"), [i.get("type") for i in req.get("input") or []],
                     tool_names(req.get("tools"))):
            return h.reply_json(400, {"error": {"type": "invalid_request_error", "code": "corpus_budget",
                                                "message": budget_message()}})
        tools, names = self.tools_to_chat(all_tools)
        body = {"model": ARGS.model, "messages": self.input_to_messages(req.get("instructions"), req.get("input"), names),
                "thinking": {"type": "enabled"}}
        if tools:
            body["tools"] = tools
        try:
            resp = chat_call(body)
        except Exception as e:  # a failed response, never the request headers
            return self.sse(h, [{"type": "response.failed", "response": {
                "id": "resp_err", "error": {"code": "upstream", "message": str(e)[:200]}}}])
        msg = (resp.get("choices") or [{}])[0].get("message") or {}
        rid = "resp_" + uuid.uuid4().hex[:24]
        evs = [{"type": "response.created", "response": {"id": rid}}]
        if msg.get("reasoning_content"):
            evs.append({"type": "response.output_item.done", "item": {
                "type": "reasoning", "id": "rs_" + uuid.uuid4().hex[:24],
                "summary": [{"type": "summary_text", "text": msg["reasoning_content"]}]}})
        if msg.get("content"):
            mid = "msg_" + uuid.uuid4().hex[:24]
            evs.append({"type": "response.output_item.added", "item": {
                "type": "message", "role": "assistant", "id": mid, "content": []}})
            evs.append({"type": "response.output_text.delta", "item_id": mid, "delta": msg["content"]})
            evs.append({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant", "id": mid,
                "content": [{"type": "output_text", "text": msg["content"]}]}})
        for tc in msg.get("tool_calls") or []:
            fn = tc.get("function") or {}
            kind, ns, name = names.get(fn.get("name"), ("function", None, fn.get("name")))
            call_id = tc.get("id") or ("call_" + uuid.uuid4().hex[:16])
            if kind == "custom":
                try:
                    inp = json.loads(fn.get("arguments") or "{}").get("input", "")
                except ValueError:
                    inp = fn.get("arguments") or ""
                item = {"type": "custom_tool_call", "call_id": call_id, "name": name, "input": inp}
            else:
                item = {"type": "function_call", "call_id": call_id, "name": name, "arguments": fn.get("arguments") or "{}"}
            if ns:
                item["namespace"] = ns
            evs.append({"type": "response.output_item.done", "item": item})
        u = resp.get("usage") or {}
        evs.append({"type": "response.completed", "response": {"id": rid, "usage": {
            "input_tokens": u.get("prompt_tokens", 0),
            "input_tokens_details": {"cached_tokens": (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0)},
            "output_tokens": u.get("completion_tokens", 0),
            "output_tokens_details": {"reasoning_tokens": (u.get("completion_tokens_details") or {}).get("reasoning_tokens", 0)},
            "total_tokens": u.get("total_tokens", 0)}}})
        self.sse(h, evs)


def lower_types(schema):
    """Gemini `parameters` schemas use OpenAPI upper-case type names (`OBJECT`); JSON Schema wants lower."""
    if isinstance(schema, dict):
        return {k: (v.lower() if k == "type" and isinstance(v, str) else lower_types(v))
                for k, v in schema.items() if k not in ("propertyOrdering", "nullable")}
    if isinstance(schema, list):
        return [lower_types(v) for v in schema]
    return schema


def part_text(p):
    if "text" in p and not p.get("thought"):
        return p["text"]
    if "inlineData" in p or "fileData" in p:
        return "[an image or file is attached; this model cannot see it]"
    return None


class Gemini:
    """The Gemini API (Gemini CLI's `GOOGLE_GEMINI_BASE_URL` gateway auth) answered from chat completions."""

    @staticmethod
    def tools_to_chat(tools):
        chat = []
        for t in tools or []:
            for d in t.get("functionDeclarations") or []:
                params = d.get("parametersJsonSchema") or d.get("parameters") or {"type": "object", "properties": {}}
                chat.append({"type": "function", "function": {
                    "name": d.get("name", ""), "description": (d.get("description") or "")[:2000],
                    "parameters": lower_types(params)}})
        return chat

    @staticmethod
    def text_of(content):
        if content is None:
            return ""
        if isinstance(content, str):
            return content
        parts = content.get("parts", []) if isinstance(content, dict) else content
        return "\n".join(t for t in (part_text(p) for p in parts if isinstance(p, dict)) if t)

    def contents_to_messages(self, system, contents):
        msgs = []
        sys_text = self.text_of(system)
        if sys_text:
            msgs.append({"role": "system", "content": sys_text})
        pending = []  # (name, id) of the last assistant turn's calls not yet answered
        for c in contents or []:
            parts = c.get("parts") or []
            if c.get("role", "user") == "model":
                calls = []
                for p in parts:
                    fc = p.get("functionCall")
                    if fc:
                        cid = fc.get("id") or ("call_" + uuid.uuid4().hex[:16])
                        calls.append({"id": cid, "type": "function", "function": {
                            "name": fc.get("name", ""), "arguments": json.dumps(fc.get("args") or {})}})
                m = {"role": "assistant", "content": "\n".join(t for t in (part_text(p) for p in parts) if t)}
                if calls:
                    m["tool_calls"] = calls
                    pending = [(x["function"]["name"], x["id"]) for x in calls]
                msgs.append(m)
                continue
            texts = []
            for p in parts:
                fr = p.get("functionResponse")
                if fr:
                    cid = fr.get("id")
                    if cid not in [i for _, i in pending]:
                        match = [x for x in pending if x[0] == fr.get("name")] or pending
                        cid = match[0][1] if match else ("call_" + uuid.uuid4().hex[:16])
                    pending = [x for x in pending if x[1] != cid]
                    resp = fr.get("response")
                    msgs.append({"role": "tool", "tool_call_id": cid,
                                 "content": resp if isinstance(resp, str) else json.dumps(resp)})
                else:
                    t = part_text(p)
                    if t:
                        texts.append(t)
            if texts:
                msgs.append({"role": "user", "content": "\n".join(texts)})
        return msgs

    @staticmethod
    def kinds(contents):
        return [next((k for k in ("functionCall", "functionResponse", "inlineData", "fileData") if k in p),
                     "thought" if p.get("thought") else "text")
                for c in contents or [] for p in c.get("parts") or []]

    def error(self, h, code, message, status):
        h.reply_json(code, {"error": {"code": code, "message": message, "status": status}})

    def handle(self, h, method, raw):
        m = re.search(r"/models/([^/:]+):(\w+)$", h.path.split("?")[0])
        if method != "POST" or not m:
            return self.error(h, 404, "not found", "NOT_FOUND")
        req = json.loads(raw or b"{}")
        asked, kind = m.group(1), m.group(2)
        if kind == "countTokens":
            chars = len(json.dumps(req.get("contents") or req.get("generateContentRequest") or {}))
            return h.reply_json(200, {"totalTokens": max(1, chars // 4)})
        if kind not in ("generateContent", "streamGenerateContent"):
            return self.error(h, 404, f"{kind} unsupported", "NOT_FOUND")
        cfg = req.get("generationConfig") or {}
        msgs = self.contents_to_messages(req.get("systemInstruction"), req.get("contents"))
        body = {"model": ARGS.model, "messages": msgs}
        thinking = (cfg.get("thinkingConfig") or {}).get("includeThoughts")
        body["thinking"] = {"type": "enabled" if thinking else "disabled"}
        schema = cfg.get("responseJsonSchema") or cfg.get("responseSchema")
        if cfg.get("responseMimeType") == "application/json" or schema:
            body["response_format"] = {"type": "json_object"}
            note = "Reply with one JSON object only."
            if schema:
                note += " It must match this JSON schema: " + json.dumps(lower_types(schema))
            msgs.append({"role": "user", "content": note})
        tools = self.tools_to_chat(req.get("tools"))
        if tools:
            body["tools"] = tools
        if not admit(h.path.split("?")[0], asked, kind == "streamGenerateContent", self.kinds(req.get("contents")),
                     [t["function"]["name"] for t in tools], json_mode=bool(schema), thinking=bool(thinking)):
            return self.error(h, 400, budget_message(), "FAILED_PRECONDITION")
        try:
            resp = chat_call(body)
        except Exception as e:  # never echo request headers
            return self.error(h, 500, "upstream: " + str(e)[:200], "INTERNAL")
        msg = (resp.get("choices") or [{}])[0].get("message") or {}
        parts = []
        if msg.get("reasoning_content") and thinking:
            parts.append({"text": msg["reasoning_content"], "thought": True})
        if msg.get("content"):
            parts.append({"text": msg["content"]})
        for tc in msg.get("tool_calls") or []:
            fn = tc.get("function") or {}
            try:
                args = json.loads(fn.get("arguments") or "{}")
            except ValueError:
                args = {}
            parts.append({"functionCall": {"name": fn.get("name", ""), "args": args,
                                           "id": tc.get("id") or ("call_" + uuid.uuid4().hex[:16])}})
        u = resp.get("usage") or {}
        out = {
            "candidates": [{"content": {"role": "model", "parts": parts or [{"text": ""}]}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": {
                "promptTokenCount": u.get("prompt_tokens", 0),
                "candidatesTokenCount": u.get("completion_tokens", 0),
                "cachedContentTokenCount": (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0),
                "thoughtsTokenCount": (u.get("completion_tokens_details") or {}).get("reasoning_tokens", 0),
                "totalTokenCount": u.get("total_tokens", 0)},
            "modelVersion": asked,
            "responseId": uuid.uuid4().hex[:24],
        }
        if kind == "generateContent":
            return h.reply_json(200, out)
        h.reply(200, b"data: " + json.dumps(out).encode() + b"\r\n\r\n", "text/event-stream")


TRANSLATORS = {"passthrough": Passthrough, "chat": Chat, "responses": Responses, "gemini": Gemini}
TRANSLATOR = None


# --- server -----------------------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def reply(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def reply_json(self, code, obj):
        self.reply(code, json.dumps(obj).encode())

    def relay(self, resp):
        """Stream an upstream response back, chunked, as it arrives."""
        with resp:
            self.send_response(resp.status if hasattr(resp, "status") else resp.code)
            for k, v in resp.headers.items():
                if k.lower() not in HOP and k.lower() != "content-encoding":
                    self.send_header(k, v)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            while True:
                chunk = resp.read1(65536) if hasattr(resp, "read1") else resp.read(65536)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()

    def serve(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        TRANSLATOR.handle(self, method, self.rfile.read(n) if n else b"")

    def do_POST(self):
        self.serve("POST")

    def do_GET(self):
        self.serve("GET")


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def server_bind(self):
        # This proxy binds loopback only. HTTPServer's reverse DNS lookup can stall startup
        # for tens of seconds when the host resolver is unavailable; no hostname is needed.
        TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]

    def handle_error(self, request, client_address):
        # A client closing its keep-alive socket is routine; anything else is worth one line.
        e = sys.exc_info()[1]
        if not isinstance(e, (ConnectionResetError, BrokenPipeError)):
            sys.stderr.write(f"key_proxy: {type(e).__name__}: {str(e)[:200]}\n")


def read_key(path):
    try:
        with open(os.path.expanduser(path)) as f:
            key = f.readline().strip()
    except OSError as e:
        sys.exit(f"key_proxy: cannot read the key file ({e.strerror})")
    if not key:
        sys.exit("key_proxy: the key file is empty")
    return key


def main():
    global ARGS, KEY, TRANSLATOR
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--translate", choices=sorted(TRANSLATORS), required=True)
    p.add_argument("--upstream", required=True)
    p.add_argument("--key-file", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--port", type=int, default=0, help="0 (the default) picks a free port")
    p.add_argument("--port-file", help="the bound port is written here once listening")
    p.add_argument("--max-calls", type=int, default=40)
    p.add_argument("--model", help="the model every call is answered by (required unless passthrough)")
    p.add_argument("--also", action="append", default=[], help="chat: a model id passed through unpinned")
    p.add_argument("--auth", choices=["bearer", "x-api-key"], default="bearer")
    p.add_argument("--thinking", choices=["on", "harness"], default="on",
                   help="chat: switch GLM thinking on unless the harness set it, or leave the request alone")
    p.add_argument("--context", type=int, default=131072, help="chat: the context length GET /models reports")
    ARGS = p.parse_args()
    if ARGS.translate != "passthrough" and not ARGS.model:
        p.error(f"--translate {ARGS.translate} needs --model")
    KEY = read_key(ARGS.key_file)
    TRANSLATOR = TRANSLATORS[ARGS.translate]()
    open(ARGS.log, "a").close()
    srv = Server(("127.0.0.1", ARGS.port), Handler)
    if ARGS.port_file:
        with open(ARGS.port_file + ".tmp", "w") as f:
            f.write(str(srv.server_address[1]))
        os.replace(ARGS.port_file + ".tmp", ARGS.port_file)
    srv.serve_forever()


if __name__ == "__main__":
    main()
