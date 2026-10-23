# Compatibility promise

Stable across minor releases from 0.18:

- Session-row field names and JSON types from `ls --json`, including the common row keys in
  `cv schema --json`.
- Message and block field names, types, discriminant values and minimal-message defaults;
  optional fields may be absent when the source records nothing.
- CLI command and argument names, types, enumerated values, requiredness and defaults, as
  `cv schema --commands --json` lists them.
- MCP tool names and their input properties, types, requiredness and defaults.
- HTTP method/path patterns of the read, search, export and task routers.
- Exit 0 on success, 1 on operational failure, 2 on a caller mistake.

New fields, arguments, tools, routes and enum values may appear: ignore unknown fields and
handle unknown enum values. Not covered: help text, human output layout, diagnostic
wording, harness extras, tool input objects, native transcript formats, HTTP bodies beyond the
session/message JSON, every query parameter, and each command's specialized output.

A replaced command, flag or route keeps a pointer for at least one minor release: CLI exit 2 or
HTTP 410, naming the replacement. An MCP tool deprecation needs a documented migration (MCP has
no 410). Removing a covered name needs a reviewed gate exception and a test of its pointer.
Names retired before 0.18 keep their documented removal versions.

## What the gate checks

CI runs `tools/check-compatibility.py --baseline <released cv> --candidate target/debug/cv`
(the last release's sha256-pinned Linux binary; separate synthetic homes), comparing the command tree, `schema --json` field lists (checked against serialized output),
`ls`/`show` JSON of a Claude and a Codex fixture, MCP `tools/list`, exits 1 and 2, and every HTTP
route in the candidate's `schema --json` `routes`: a status the baseline answered must hold.
Additions pass; removed or changed shapes and defaults fail. Route removal is caught once 0.18.0 is
the baseline (older ones list no routes).

A reviewed removal is an `--accept <contract path>` on the CI step until the baseline moves.
0.18 accepts MCP `workflow`'s `follow` (now `for`); `rewind --from-compaction`, `deferrals
--since` and `dataset --out` (CLI and MCP); `watch --interval`'s value type; and
`rows.codex.message_count` (`null` when uncounted). `crates/cv/tests/announce.rs` tests pipe and
terminal behavior; `python3 -I tools/test-compatibility.py` runs the comparator's mutation tests.
