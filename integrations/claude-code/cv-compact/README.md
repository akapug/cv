# cv-compact

A Claude Code plugin that serves a session's compaction from cv's ready summary
(`cv watch --kind precompact`), inside Claude Code's own compaction (the `session.compact` event,
Claude Code 2.1.312 and later). On any miss, error or the deadline, Claude Code compacts as its
own. Every compaction, cv's or Claude Code's, is recorded for `cv watch --kind precompact report`.

Run it on a seat with the plugin folder loaded (`claude --plugin-dir <this folder>`, or
`CLAUDE_CODE_PLUGIN_DIRS`), and the follower running on the same host
(`cv watch --kind precompact`). Options (under `pluginConfigs` in the seat's settings):

- `serve`: `host` (cv's `[precompact] serve` decides; the default), `native`, or `cv`;
- `cv`: the cv binary (default `cv` on PATH);
- `deadline_ms`: how long the commit may take before Claude Code compacts as its own (5000).

Tests: `claude plugin test <this folder>`. Design: `docs/design/PRECOMPACT.md`.
