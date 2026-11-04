# cv-compact

A Claude Code plugin that serves a session's compaction from cv's ready summary
(`cv watch --kind precompact`), inside Claude Code's own compaction (the `session.compact` event,
Claude Code 2.1.312 and later). On any miss, error or the deadline, Claude Code compacts as its
own. Every compaction, cv's or Claude Code's, is recorded for `cv watch --kind precompact report`.

On a seat that serves cv's, the `precompute` trigger asks cv first (`commit --check`): where cv would
serve, Claude Code's background summary is skipped, so it is neither waited for nor paid for; where
cv would not, it runs and stays the miss path's fast answer.

A served compaction fires no `PreCompact`. A tool that hangs work on that hook reads `PostCompact`
instead: `compact_summary` is empty where cv served and holds the summary where Claude Code wrote it.

Run it on a seat with the plugin folder loaded (`claude --plugin-dir <this folder>`, or
`CLAUDE_CODE_PLUGIN_DIRS`), and the follower running on the same host
(`cv watch --kind precompact`). Options (under `pluginConfigs` in the seat's settings):

- `serve`: `host` (cv's `[precompact] serve` decides; the default), `native`, or `cv`;
- `cv`: the cv binary (default `cv` on PATH);
- `deadline_ms`: how long the commit may take before Claude Code compacts as its own (5000).

Tests: `claude plugin test <this folder>`. Design: `docs/design/PRECOMPACT.md`.
