#!/usr/bin/env python3
"""Make a precompaction replay fixture from a real compaction: the transcript's current window and
the compaction request the harness sent for it, both redacted the same way.

    precompact-fixture.py TRANSCRIPT BYTES REQUEST.json OUT_DIR NAME

TRANSCRIPT is a Claude Code session file, BYTES the prefix length the request was built from (the
transcript as it stood when the harness compacted), REQUEST.json the captured request
(`{"headers": ..., "body": ...}` or a bare body). Writes OUT_DIR/NAME.jsonl (the window: records
after the last compaction boundary, plus the boundary and summary pair that opens it) and
OUT_DIR/NAME.request.json (the body without its system prompt and tool schemas).

Redaction keeps what the replay tests and nothing else: every run of letters or digits in every
string value is replaced by a pseudo-word of the same length and character classes, derived from a
keyed hash of the run, so equal text stays equal on both sides and a substring stays a substring;
whitespace, punctuation and markup shape (`<lowercase-name>…</lowercase-name>`) survive. Structural
values (types, roles, timestamps, flags) are kept; ids are swapped for short ones, the same on both
sides.

Size is cut the same way on both sides, so what matches in the real pair matches in the fixture:
every text block keeps its first TEXT_CHARS characters (default 160), tool results their first 48,
tool calls lose their input, thinking and image data go. Text never shrinks below the anchor
minimum, so each text block still yields its anchors.
"""
import hashlib, json, os, re, sys

KEEP = {
    "type", "role", "subtype", "uuid", "parentUuid", "logicalParentUuid", "sessionId",
    "timestamp", "id", "tool_use_id", "stop_reason", "level", "trigger", "name",
    "version", "promptSource", "turnOrigin", "kind", "producer", "operation", "media_type", "model",
}
USAGE = {"input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"}
RUN = re.compile(r"[A-Za-z0-9]+")
KEY = os.environ.get("PRECOMPACT_FIXTURE_KEY", "cv-precompact-fixture").encode()
RESULT_CHARS = 48
TEXT_CHARS = int(os.environ.get("PRECOMPACT_FIXTURE_TEXT_CHARS", "160"))
LOWER, UPPER, DIGIT = "abcdefghijklmnopqrstuvwxyz", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "0123456789"


def pseudo(run):
    digest = hashlib.sha256(KEY + run.encode()).digest()
    out = []
    for i, ch in enumerate(run):
        b = digest[i % len(digest)] + i
        out.append(LOWER[b % 26] if ch.islower() else UPPER[b % 26] if ch.isupper() else DIGIT[b % 10])
    return "".join(out)


def redact_text(s):
    return RUN.sub(lambda m: pseudo(m.group(0)), s)


IDS = {"uuid", "parentUuid", "logicalParentUuid", "id", "tool_use_id"}
SHORT = {}


def short(v):
    # One short id per long one, the same on both sides.
    return SHORT.setdefault(v, f"i{len(SHORT)}")


def redact(v, key=None):
    if isinstance(v, str) and key in IDS:
        return short(v)
    if isinstance(v, dict):
        return {k: redact(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [redact(x, key) for x in v]
    if isinstance(v, str) and key not in KEEP:
        return redact_text(v)
    return v


def clip_tool_results(content):
    if isinstance(content, str):
        return content[:TEXT_CHARS]
    if not isinstance(content, list):
        return content
    out = []
    for part in content:
        if isinstance(part, dict) and part.get("type") == "text":
            part = dict(part, text=part.get("text", "")[:TEXT_CHARS])
        if isinstance(part, dict) and part.get("type") == "tool_use":
            part = dict(part, input={})
        if isinstance(part, dict) and part.get("type") == "tool_result":
            part = dict(part)
            c = part.get("content")
            if isinstance(c, list):
                c = "\n".join(p.get("text", "") for p in c if isinstance(p, dict))
            part["content"] = (c or "")[:RESULT_CHARS]
        if isinstance(part, dict) and part.get("type") in ("thinking", "redacted_thinking"):
            continue
        if isinstance(part, dict) and part.get("type") == "image":
            part = {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ""}}
        out.append(part)
    return out


def main():
    transcript, upto, request, out_dir, name = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
    with open(transcript, "rb") as f:
        lines = f.read(upto).decode("utf-8").splitlines()
    records = []
    for line in lines:
        try:
            records.append(json.loads(line))
        except ValueError:
            continue
    start = 0
    for i, r in enumerate(records):
        if r.get("subtype") == "compact_boundary":
            start = i
    window = []
    for r in records[start:]:
        if r.get("type") not in ("user", "assistant", "system"):
            continue
        if r.get("isSidechain"):
            continue
        r = {k: v for k, v in r.items() if k in KEEP or k in ("message", "isMeta", "isCompactSummary", "isApiErrorMessage", "compactMetadata", "origin", "isVisibleInTranscriptOnly", "content")}
        if isinstance(r.get("message"), dict):
            m = dict(r["message"])
            m["content"] = clip_tool_results(m.get("content"))
            keep = {"role", "content", "model", "usage", "stop_reason", "id", "type"}
            if isinstance(m.get("usage"), dict):
                m["usage"] = {k: v for k, v in m["usage"].items() if k in USAGE}
            r["message"] = {k: v for k, v in m.items() if k in keep}
        window.append(redact(r))
    captured = json.load(open(request))
    body = captured.get("body", captured)
    headers = {k: v for k, v in (captured.get("headers") or {}).items() if k.lower() in ("x-claude-code-session-id", "user-agent")}
    body = {k: v for k, v in body.items() if k in ("model", "messages", "metadata", "max_tokens", "stream")}
    session = json.loads(body.pop("metadata", {}).get("user_id") or "{}").get("session_id")
    body["messages"] = [dict(m, content=clip_tool_results(m.get("content"))) for m in body["messages"]]
    body = redact(body)
    if session:
        body["metadata"] = {"user_id": json.dumps({"session_id": session})}
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, name + ".jsonl"), "w") as f:
        for r in window:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(os.path.join(out_dir, name + ".request.json"), "w") as f:
        json.dump({"headers": headers, "body": body}, f, ensure_ascii=False)
    print(f"{name}: {len(window)} records, {len(body['messages'])} request messages")


if __name__ == "__main__":
    main()
