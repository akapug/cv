# Compatibility promise

Starting with 0.18, scripts can rely on these surfaces across minor releases:

- Session-row field names and JSON types from `ls --json`, including the common row
  keys listed by `cv schema --json`.
- Message and block field names, types, discriminant values, and the existing minimal
  message defaults. Optional fields may be absent when the source records no value.
- Visible CLI command/argument names, argument types, accepted enumerated values,
  requiredness and existing defaults described by `cv schema --commands --json`.
- MCP tool names and existing input properties, types, requiredness and defaults.
- HTTP API method/path patterns accepted by the read, search, export and task routers.
- Exit 0 for success, 1 for operational failure, and 2 for caller mistakes.

Additive fields, optional arguments, tools, routes, and enum values are compatible.
Consumers should ignore fields they do not use and handle unfamiliar enum values.
Help text, human output layout, diagnostic wording, opaque harness extras, tool input
objects, and native transcript formats are outside this promise. HTTP response bodies
beyond the shared session/message JSON are not covered by this first gate; neither
are every query parameter nor every command's specialized output format.

When a command/flag or route is replaced, it keeps a pointer for at least one minor
release: CLI exit 2 or HTTP 410, naming the replacement. MCP tool deprecations need an
explicit documented migration because MCP has no HTTP 410 equivalent. Removing a
covered name requires an exact, reviewed gate exception and a test of the pointer;
changing the baseline to hide a removal is not an exception. Already retired names
remain governed by their documented removal versions.

## What the gate checks

CI downloads the last release's Linux binary (sha256-pinned in `ci.yml`) and runs
`tools/check-compatibility.py --baseline <released cv> --candidate target/debug/cv`.
Nothing is rebuilt from a tag; each binary reports its own contracts, in separate
synthetic homes, and the comparator permits additions and prose changes while
rejecting removals and changed existing shapes or defaults.

It compares the command tree (`schema --commands --json`), the data schema's field and
vocabulary lists (`schema --json`), the actual `ls`/`show` JSON shapes of fixture
sessions, the real MCP `tools/list`, and the HTTP routes. The route inventory is the
candidate's `schema --json` `routes` list, which the routers' own tables produce (the
`routes!` macro spells each arm once), so it cannot drift from the code; both servers
are probed on every pattern and a status the baseline answered must hold. A baseline
older than 0.18 lists no routes and is probed with the candidate's list, so a removed
route is first caught once 0.18.0 is the baseline. Exit 2 for a caller mistake and
exit 1 for an operational failure are exercised on both binaries. The schema's field
lists are also checked against the serialized output. These are fixtures and
inventories, not a proof of semantic equivalence for every transcript.

A reviewed removal is named on the CI step with `--accept <contract path>`, beside the
pinned baseline that still has it; the exception leaves with that baseline. The 0.18
entries are MCP `workflow`'s unbounded `follow`, replaced by bounded `for`; the renamed
`rewind --from-compaction`, `deferrals --since` and `dataset --out`, on the CLI and in MCP;
`watch --interval`'s widened value type; and `rows.codex.message_count`, an uncounted
session's `null`. The fixture holds a Claude session (counted at discovery) and a Codex one
(counted on a complete read), so the session-row shape is checked both ways. CLI
pipe/terminal behavior is covered separately by `crates/cv/tests/announce.rs`.

Run the comparator's mutation tests with `python3 -I tools/test-compatibility.py`. Before
accepting a contract change, inspect the actual failure and add only its narrow
`--accept` and pointer test when a deprecation was intended.
