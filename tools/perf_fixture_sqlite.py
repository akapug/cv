"""Deterministic SQLite scale inputs using adapter-supported native schemas.

Synthetic payloads and IDs only; never copies or rewrites corpus versions. Each row is
inserted separately, so generation itself need not retain a logical session in memory.
"""
import hashlib
import importlib.util
import json
import sqlite3
from pathlib import Path

FORMATS = ("devin", "hermes", "goose", "opencode", "cursor")
SID = "00000000-0000-4000-8000-000000000001"
REPO = Path(__file__).resolve().parent.parent


def generate_sqlite(harness, root, size, records, escaped, shape):
    allowed = {"devin": ("history", "metadata", "unselected"),
               "hermes": ("history", "prompt-only"), "goose": ("history", "selected", "metadata"),
               "opencode": ("history", "parts", "selected", "selected-v2", "shell-v2"), "cursor": ("history", "inline", "legacy", "metadata", "selected", "selected-ide")}
    if shape not in allowed[harness]:
        raise ValueError(f"{harness} supports shapes {allowed[harness]}")
    if harness == "cursor" and shape in ("metadata", "selected"):
        return generate_cursor_cli(root, size, records, escaped, shape)
    name = {"devin": "sessions.db", "hermes": "state.db", "goose": "sessions.db",
            "opencode": "opencode.db", "cursor": "globalStorage/state.vscdb"}[harness]
    if harness == "cursor" and shape == "legacy": name = "workspaceStorage/perf/state.vscdb"
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    pattern = 'line\n"quoted"\t\\end' if escaped else "plain payload "
    def payload(i):
        n = size // records + (i < size % records)
        # Distinct prompts exercise retention rather than duplicate-message suppression.
        prefix = f"record {i}: "
        return (prefix + pattern * ((n + len(pattern) - 1) // len(pattern)))[:n]
    def j(value):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    with sqlite3.connect(path) as db:
        if harness == "devin":
            spec = importlib.util.spec_from_file_location("devin_fixture_schema", REPO / "tools/harness-fixtures/devin/generate.py")
            schema = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(schema)
            db.executescript(schema.SCHEMA)
            db.execute("INSERT INTO sessions(id,working_directory,backend_type,model,agent_mode,created_at,last_activity_at,title,main_chain_id) VALUES(?, '/perf','synthetic','fixture-model','agent',1767225600,1767225601,'scale fixture',?)", (SID, 1 if shape == "unselected" else records))
            for i in range(records):
                body = payload(i)
                chat = {"message_id": f"m{i}", "role": "assistant", "content": body if shape == "history" else "small reply"}
                metadata = {"summarized_from": None, "num_tokens_preceding": i, "is_system_prefix": False}
                if shape != "history":
                    metadata["extensions"] = {"synthetic/scale-payload": body}
                db.execute("INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at,metadata) VALUES(?,?,?,?,?,?)",
                           (SID, i + 1, None if i == 0 or shape == "unselected" else i, j(chat), 1767225600 + i, j(metadata)))
        elif harness == "hermes":
            db.executescript((REPO / "crates/cv-core/tests/fixtures/hermes/schema-v30.sql").read_text())
            db.execute("CREATE INDEX perf_messages_session_id ON messages(session_id,id)")
            db.execute("INSERT INTO sessions(id,source,started_at,title) VALUES(?,'cli',1767225600,'scale fixture')", (SID,))
            for i in range(records):
                db.execute("INSERT INTO messages(session_id,role,content,timestamp,active,compacted) VALUES(?,?,?,?,1,0)",
                           (SID, "user" if shape == "prompt-only" else "assistant", payload(i), 1767225600 + i))
        elif harness == "goose":
            # Both real corpus versions (1.51.0 and 1.54.0) include this empty extension state.
            db.executescript("CREATE TABLE sessions(id TEXT PRIMARY KEY,name TEXT,working_dir TEXT,created_at TEXT,updated_at TEXT,extension_data TEXT DEFAULT '{}'); CREATE TABLE messages(id INTEGER PRIMARY KEY,session_id TEXT,role TEXT,content_json TEXT,created_timestamp INTEGER); CREATE INDEX messages_session ON messages(session_id,created_timestamp,id);")
            db.execute("INSERT INTO sessions(id,name,working_dir,created_at,updated_at) VALUES(?,'scale fixture','/perf','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')", (SID,))
            if shape == "metadata":
                db.execute("ALTER TABLE messages ADD COLUMN metadata_json TEXT")
                for i in range(records):
                    request = [{"type": "toolRequest", "id": f"call-{i}", "toolCall": {"status": "success",
                                "value": {"name": "shell", "arguments": {"command": "fixture"}}}}]
                    result = [{"type": "toolResponse", "id": f"call-{i}", "toolResult": {"status": "success",
                               "value": {"content": [{"type": "text", "text": "tiny result"}]}}}]
                    metadata = {"userVisible": True, "agentVisible": True, "operations": [{"ignoredPayload": payload(i)}],
                                "usage": {"input_tokens": 10, "output_tokens": 2}}
                    db.execute("INSERT INTO messages VALUES(?,?,'assistant',?,?,?)", (2 * i, SID, j(request), 1767225600 + 2 * i, j(metadata)))
                    db.execute("INSERT INTO messages VALUES(?,?,'user',?,?,?)", (2 * i + 1, SID, j(result), 1767225601 + 2 * i, "{}"))
            elif shape == "selected":
                for i in range(records):
                    text = payload(i)
                    quarter = len(text) // 4
                    request = [{"type": "text", "text": text[:2 * quarter]},
                               {"type": "toolRequest", "id": f"call-{i}", "toolCall": {"status": "success",
                                "value": {"name": "shell", "arguments": {"command": text[2 * quarter:3 * quarter]}}}}]
                    result = [{"type": "toolResponse", "id": f"call-{i}", "toolResult": {"status": "success",
                               "value": {"content": [{"type": "text", "text": text[3 * quarter:]},
                                                        {"type": "resource", "resource": {"text": "marker"}}]}}}]
                    db.execute("INSERT INTO messages VALUES(?,?,'assistant',?,?)", (2 * i, SID, j(request), 1767225600 + 2 * i))
                    db.execute("INSERT INTO messages VALUES(?,?,'user',?,?)", (2 * i + 1, SID, j(result), 1767225601 + 2 * i))
                final = [{"type": "toolRequest", "id": "final", "toolCall": {"status": "success",
                          "value": {"name": "gate-final", "arguments": {}}}}]
                db.execute("INSERT INTO messages VALUES(?,?,'assistant',?,?)", (2 * records, SID, j(final), 1767225600 + 2 * records))
            else:
                for i in range(records):
                    db.execute("INSERT INTO messages VALUES(?,?, 'assistant',?,?)", (i, SID, j([{"type": "text", "text": payload(i)}]), 1767225600 + i))
        elif harness == "opencode" and shape == "shell-v2":
            db.executescript("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,time_updated INTEGER,data TEXT); CREATE INDEX messages_session ON session_message(session_id,seq);")
            db.execute("INSERT INTO session_v2 VALUES(?,'/perf','shell fixture',1,2)", (SID,))
            for i in range(records):
                body = payload(i)
                data = dict(time=dict(created=i + 1), shellID=f"shell-{i}", command="echo user-shell",
                            status="exited", exit=0,
                            output=dict(output=body, cursor=1, size=len(body.encode()), truncated=False),
                            metadata=dict(background=True))
                db.execute("INSERT INTO session_message VALUES(?,?,'shell',?,?,?,?)",
                           (f"m{i:09d}", SID, i, i + 1, i + 1, j(data)))
        elif harness == "opencode" and shape == "selected-v2":
            db.executescript("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,time_updated INTEGER,data TEXT); CREATE INDEX messages_session ON session_message(session_id,seq);")
            db.execute("INSERT INTO session_v2 VALUES(?,'/perf','selected fixture',1,2)", (SID,))
            for i in range(records + 1):
                if i == records:
                    content = [{"type": "tool", "id": "final", "name": "gate-final",
                                "state": {"status": "running", "input": {}}}]
                else:
                    text = payload(i)
                    quarter = len(text) // 4
                    content = [{"type": "text", "text": text[:quarter]},
                               {"type": "reasoning", "text": text[quarter:2 * quarter]},
                               {"type": "tool", "id": f"call-{i}", "name": "read", "state": {
                                   "status": "completed", "input": {"payload": text[2 * quarter:3 * quarter]},
                                   "content": [{"type": "text", "text": text[3 * quarter:]}]}}]
                db.execute("INSERT INTO session_message VALUES(?,?,'assistant',?,?,?,?)",
                           (f"m{i:09d}", SID, i, i + 1, i + 1, j({"content": content})))
        elif harness == "opencode":
            db.executescript("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,data TEXT); CREATE INDEX message_session ON message(session_id,time_created,id); CREATE INDEX part_message ON part(message_id,id);")
            db.execute("INSERT INTO session VALUES(?,'/perf','scale fixture',1767225600000,1767225600000)", (SID,))
            for i in range(records):
                mid = "m0" if shape == "parts" else f"m{i:09d}"
                if i == 0 or shape != "parts":
                    db.execute("INSERT INTO message VALUES(?,?,?,?)", (mid, SID, 1767225600000 + i, j({"role": "assistant", "time": {"created": 1767225600000 + i}})))
                if shape == "selected":
                    text = payload(i)
                    quarter = len(text) // 4
                    values = [{"type": "text", "text": text[:quarter]},
                              {"type": "reasoning", "text": text[quarter:2 * quarter]},
                              {"type": "tool", "callID": f"call-{i}", "tool": "read", "state": {
                                  "status": "completed", "input": {"payload": text[2 * quarter:3 * quarter]},
                                  "output": {"payload": text[3 * quarter:]}}}]
                    for part, value in enumerate(values):
                        db.execute("INSERT INTO part VALUES(?,?,?)", (f"p{i:09d}-{part}", mid, j(value)))
                else:
                    db.execute("INSERT INTO part VALUES(?,?,?)", (f"p{i:09d}", mid, j({"type": "text", "text": payload(i)})))
            if shape == "selected":
                db.execute("INSERT INTO message VALUES('final',?,?,?)", (SID, 1767225600000 + records, j({"role": "assistant"})))
                db.execute("INSERT INTO part VALUES('final','final',?)", (j({"type": "tool", "callID": "final", "tool": "gate-final", "state": {"status": "running", "input": {}}}),))
        else:
            table = "ItemTable" if shape == "legacy" else "cursorDiskKV"
            db.execute(f"CREATE TABLE {table}(key TEXT PRIMARY KEY,value TEXT)")
            # Stream the inline JSON to a temporary generator file to avoid a whole Python tree.
            composer = {"name": "scale fixture", "createdAt": 1767225600000, "modelConfig": {"modelName": "fixture-model"}}
            if shape in ("inline", "legacy"):
                parts = root / "composer-generation.json"
                with parts.open("w") as out:
                    out.write('{"tabs":[{"tabId":"scale-tab","chatTitle":"scale fixture","bubbles":[' if shape == "legacy" else j(composer)[:-1] + ',"conversation":[')
                    for i in range(records):
                        if i: out.write(',')
                        out.write(j({"bubbleId": f"b{i}", "type": "ai" if shape == "legacy" else 2, "text": payload(i)}))
                    out.write(']}]}' if shape == 'legacy' else ']}')
                # Native stores accept BLOB values too. Allocate the cell without a giant
                # Python/String/SQLite binding copy, then fill it through SQLite's blob API.
                key = "workbench.panel.aichat.view.aichat.chatdata" if shape == "legacy" else f"composerData:{SID}"
                db.execute(f"INSERT INTO {table} VALUES(?,zeroblob(?))", (key, parts.stat().st_size))
                rowid = db.execute(f"SELECT rowid FROM {table} WHERE key=?", (key,)).fetchone()[0]
                with db.blobopen(table, "value", rowid) as blob, parts.open("rb") as source:
                    while chunk := source.read(65536):
                        blob.write(chunk)
                parts.unlink()
            elif shape == "selected-ide":
                composer.update(composerId=SID, unifiedMode="agent", lastUpdatedAt=1767225601000)
                composer["fullConversationHeadersOnly"] = [{"bubbleId": f"b{i}"} for i in range(records + 1)]
                db.execute("INSERT INTO cursorDiskKV VALUES(?,?)", (f"composerData:{SID}", j(composer)))
                for i in range(records + 1):
                    text = payload(i) if i < records else ""
                    quarter = len(text) // 4
                    bubble = {"bubbleId": f"b{i}", "type": 2, "text": text[:quarter],
                              "modelInfo": {"modelName": "fixture-model"}, "unifiedMode": 1,
                              "thinking": {"text": text[quarter:2 * quarter], "signature": "sig"},
                              "toolFormerData": {"name": "gate-final" if i == records else "run_terminal_cmd",
                                  "toolCallId": f"call-{i}", "rawArgs": j({"command": text[2 * quarter:3 * quarter]}),
                                  "status": "running" if i == records else "success", "result": text[3 * quarter:]}}
                    db.execute("INSERT INTO cursorDiskKV VALUES(?,?)", (f"bubbleId:{SID}:b{i}", j(bubble)))
            else:
                composer["fullConversationHeadersOnly"] = [{"bubbleId": f"b{i}"} for i in range(records)]
                db.execute("INSERT INTO cursorDiskKV VALUES(?,?)", (f"composerData:{SID}", j(composer)))
                for i in range(records):
                    db.execute("INSERT INTO cursorDiskKV VALUES(?,?)", (f"bubbleId:{SID}:b{i}", j({"bubbleId": f"b{i}", "type": 2, "text": payload(i)})))
    count = records * 2 if harness == "goose" and shape == "metadata" else records + 1 if shape == "selected-ide" else records * 2 + 1 if shape in ("selected", "selected-v2") else 1 if shape in ("unselected", "parts") else records
    return {"harness": harness, "id": "legacy:scale-tab" if harness == "cursor" and shape == "legacy" else SID, "root": str(root), "path": str(path), "shape": shape,
            **({"decoded_metadata_payload_bytes": size, "expected_compactions": 0} if harness == "goose" and shape == "metadata" else {}),
            "requested_payload_bytes": size, "decoded_payload_bytes": size, "selected_payload_bytes": size, "records": records, "expected_messages": count,
            "escaped": escaped, "physical_input_bytes": path.stat().st_size,
            "synthetic": True, "provenance": "adapter-supported native schema; made-up IDs and payloads"}


def generate_cursor_cli(root, size, records, escaped, shape):
    """Native CLI blobs/meta with tiny visible bodies and large unused provider sidecars.

    Entirely synthetic records use the installed wire shapes; no corpus store is copied.
    Payload bytes count sidecars once, distinct from encoded JSON/SQLite source size.
    """
    path = root / "chats" / "perf" / SID / "store.db"
    path.parent.mkdir(parents=True)
    pattern = 'line\n"quoted"\t\\end' if escaped else "plain payload "

    def encode(number):
        result = bytearray()
        while number > 127:
            result.append((number & 127) | 128)
            number >>= 7
        result.append(number)
        return bytes(result)

    def field(number, value):
        return encode(number << 3 | 2) + encode(len(value)) + value

    with sqlite3.connect(path) as db:
        db.executescript("CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT);")
        ids = []

        def message(value):
            raw = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
            key = hashlib.sha256(raw).hexdigest()
            db.execute("INSERT INTO blobs VALUES(?,?)", (key, raw))
            ids.append(key)

        message({"role": "user", "content": "injected context",
                 "providerOptions": {"cursor": {"requestContextCompleteness": False}}})
        for i in range(records):
            length = size // records + (i < size % records)
            payload = (pattern * ((length + len(pattern) - 1) // len(pattern)))[:length]
            split = length // 2
            if shape == "selected":
                quarter = length // 4
                message({"role": "assistant", "content": [{"type": "text", "text": payload[:quarter]},
                         {"type": "reasoning", "text": payload[quarter:2 * quarter]},
                         {"type": "tool-call", "toolCallId": f"c{i}", "toolName": "Read",
                          "args": {"payload": payload[2 * quarter:3 * quarter]}}]})
                message({"role": "tool", "content": [{"type": "tool-result", "toolCallId": f"c{i}",
                         "toolName": "Read", "result": payload[3 * quarter:]}]})
                continue
            message({"role": "assistant", "content": [{"type": "tool-call", "toolCallId": f"c{i}",
                     "toolName": "Bash", "args": {"command": "true"}}], "providerOptions": {"cursor": {
                     "systemPromptFingerprint": {"model": "test"}, "ignoredPayload": payload[:split]}}})
            message({"role": "tool", "content": [{"type": "tool-result", "toolCallId": f"c{i}",
                     "toolName": "Bash", "result": f"result {i}"}], "providerOptions": {"cursor": {
                     "highLevelToolCallResult": {"isError": True, "output": payload[split:]}}}})
        if shape == "selected":
            message({"role": "assistant", "content": [{"type": "tool-call", "toolCallId": "final",
                     "toolName": "gate-final", "args": {}}]})
        message({"role": "assistant", "content": "native summary",
                 "providerOptions": {"cursor": {"isSummary": True}}})
        native_root = b"".join(field(1, bytes.fromhex(key)) for key in ids)
        native_root += field(9, b"file:///perf") + encode(10 << 3) + encode(1)
        key = hashlib.sha256(native_root).hexdigest()
        db.execute("INSERT INTO blobs VALUES(?,?)", (key, native_root))
        meta = {"agentId": SID, "latestRootBlobId": key, "name": "facts scale fixture",
                "mode": "default", "createdAt": 1}
        db.execute("INSERT INTO meta VALUES('0',?)", (json.dumps(meta, separators=(",", ":")).encode().hex(),))
    return {"harness": "cursor", "format": "cursor-cli", "id": "cli:" + SID,
            "path": str(path), "root": str(root), "shape": shape, "records": records,
            "expected_messages": records * 2 + 2 + (shape == "selected"), "expected_compactions": 0,
            "decoded_payload_bytes": size, "selected_payload_bytes": size,
            **({"decoded_metadata_payload_bytes": size, "payload_location": "metadata"} if shape == "metadata" else {"payload_location": "selected"}),
            "physical_input_bytes": path.stat().st_size, "escaped": escaped,
            "synthetic": True, "provenance": "synthetic native CLI blobs/meta schema"}
