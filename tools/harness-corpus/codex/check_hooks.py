#!/usr/bin/env python3
"""Refuse a hooks fixture unless all five live loops produced their expected effects."""
import json
import sys
import subprocess
from pathlib import Path

rows = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
ledger = [json.loads(line) for line in Path(sys.argv[2]).read_text().splitlines()]
assert all(row["exit_code"] == 0 and not row["stderr"] for row in rows), "hook failed"

def outputs(event, source=None):
    for row in rows:
        payload = row["input"]
        if payload["hook_event_name"] == event and (source is None or payload.get("source") == source):
            yield payload, json.loads(row["stdout"]) if row["stdout"] else {}

def context(output):
    return output.get("hookSpecificOutput", {}).get("additionalContext", "")

assert any("earlier session(s)" in context(out) for _, out in outputs("SessionStart", "startup")), "start never packed earlier sessions"
assert any("decision(s) owed" in context(out) for _, out in outputs("UserPromptSubmit")), "decision reminder never fired"
assert any("compaction #1" in context(out) and "--pre-compaction 1" in context(out) for _, out in outputs("SessionStart", "compact")), "compact never read the current native boundary"
assert any(payload.get("tool_name") == "apply_patch" and out.get("hookSpecificOutput", {}).get("permissionDecision") == "deny" for payload, out in outputs("PreToolUse")), "patch guard never blocked"
starts = [row for row in ledger if row["event"] == "start"]
stops = [row for row in ledger if row["event"] == "stop"]
assert any(start["session_id"] == stop["session_id"] and start["agent_id"] == stop["agent_id"] and start.get("agent_transcript_path") == stop.get("agent_transcript_path") and stop.get("transcript_path") for start in starts for stop in stops), "lane identities or transcript paths did not compose"
cv = sys.argv[3]
subprocess.run([cv, "ls", "--harness", "codex", "--json"], check=True, stdout=subprocess.DEVNULL)
assert any(any(lane["agent_id"] == start["agent_id"] and lane["status_source"] == "hook_ledger" and lane["status"] == "stopped"
               for lane in json.loads(subprocess.check_output([cv, "lanes", "codex:" + start["session_id"], "--json"]))) for start in starts), "recorded lane did not surface as stopped"
compact = next(payload for payload, _ in outputs("SessionStart", "compact"))
recovered = subprocess.check_output([cv, "show", "codex:" + compact["session_id"], "--pre-compaction", "1", "--json"])
assert json.loads(recovered)["messages"], "compaction recovery returned no context"
print("hooks: five live loops and readback verified")
