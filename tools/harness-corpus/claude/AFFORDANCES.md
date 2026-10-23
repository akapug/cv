# Claude Code affordances and what the corpus captures (2.1.296, 2.1.304)

Enumerated from the binary's tool registry (`claude -p --output-format stream-json --verbose`
prints the `system/init` tool list without a model call; `--tools` with every name shows which can
be enabled at all), its `--help`, and the records the corpus run wrote. "Headless" means `claude -p`
against a third-party endpoint (Z.ai), which never receives Anthropic's remote feature flags.

The IR column is how cv reads it today (`MessageKind`/`Block`/`extra["claude"]`/side readers).

| affordance | tool / record | persisted as | in corpus | cv IR today |
|---|---|---|---|---|
| prompt, reply | `user` / `assistant` | transcript lines (one per content block, shared `message.id`) | yes | `Prompt`/`Human`; `Reply` with `Text` |
| thinking | `thinking` block | assistant line; `thinkingDisplay`, `thinkingDurationMs` | yes (GLM thinks every turn) | `Block::Thinking` |
| resume | `--resume` | appended to the same file; `queue-operation` enqueue/dequeue | yes | same session; queue carried |
| fork a session | `--resume --fork-session` | a new file, every record re-stamped with the new `sessionId`, no back pointer | yes | `lineage.forked_from`, inferred: the fork's head is copied history (older than the record before it, or a `compact_boundary`), and a sibling transcript holds that `uuid` |
| session name | `-n` | `custom-title`, `agent-name` | yes | session title; `extra.claude.agent_name` |
| model switch | `--model` on resume | `attachment.model`, `message.model` changes | yes | `InjectedContext` + per-message `model`; no `ModelChange` |
| compaction | `/compact` | `system.compact_boundary` + `isCompactSummary` user turn + slash-command rows | yes | `CompactionBoundary`, `CompactionSummary`, `Notice` |
| CLAUDE.md memory | project `CLAUDE.md` | `attachment.instructions` | yes | `InjectedContext` |
| auto-memory | `projects/<p>/memory/` | an empty dir unless the model writes there | no (dir only) | not read |
| hooks | settings `hooks` | `attachment.hook_success`, `attachment.hook_additional_context`, `system.stop_hook_summary` | yes (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop) | `InjectedContext`/`Hook`; stop summary dropped when silent |
| Read/Edit/Write/Bash | built-in tools | `tool_use` + `tool_result` + `toolUseResult` sidecar | yes | `ToolUse`/`ToolResult`, sidecar in `details` |
| parallel tool calls | two `tool_use` in one message | consecutive assistant lines | yes | two `ToolUse` |
| tool error | `is_error: true` | tool_result + string `toolUseResult` | yes | `ToolResult` with error |
| permission denial | `--permission-mode dontAsk` | tool_result "Permission to use Bash has been denied" | yes | `ToolResult` (error); `approval/deny` |
| large result spill | `BASH_MAX_OUTPUT_LENGTH` | `<sessionId>/tool-results/<id>.txt` + `<persisted-output>` stub | yes | `details.persistedOutput` |
| notebook | `NotebookEdit` | tool_use/result | yes | `ToolUse` |
| file checkpoints / rewind | `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` | `file-history-snapshot`, `file-history-delta`, `file-history/<sid>/<hash>@vN` | yes (records; `/rewind` itself is interactive) | carried; `checkpoint/state` (a tally: points, files) |
| image input | stream-json user `image` block | user content block | yes | `Block::Image` |
| image via tool | `Read` on a png | image inside `tool_result` content; copy under `$CLAUDE_CODE_TMPDIR/.../images/` | yes | nested in `ToolResult` |
| plan mode | `--permission-mode plan` | `permissionMode: plan` on turns, `attachment.plan_mode`, `mode` | yes | `InjectedContext`; `permissionMode` → `approval/state` |
| ExitPlanMode / EnterPlanMode | tools | — | no: not offered headless | — |
| task list | `TaskCreate`/`TaskUpdate`/`TaskList`/`TaskGet` (`CLAUDE_CODE_ENABLE_TODO_TOOLS`) | tool calls + `tasks/<sessionId>/<n>.json` | yes | `ToolUse`; `tasks/` not read |
| TodoWrite (v1) | `TodoWrite` | — | no: 2.1.296 offers the Task* tools instead | — |
| sub-agent | `Agent` (`Task`) | `subagents/agent-<id>.jsonl` + `.meta.json` (`agentType`, `toolUseId`, `spawnDepth`, `requestShape`) | yes (custom `echoer`, `general-purpose`) | child session via `subagent_tree`, `lineage.parent`/`spawned_by_tool_use`; no `SubagentSpawn` message |
| background agent + notification | `Agent run_in_background` | async launch result, then a `<task-notification>` user turn (`origin.kind: task-notification`, `turnOrigin: task_notification`) | yes | `Prompt` with `origin: harness`; status via `task_notices` |
| resume a sub-agent | `SendMessage` | tool call; the child file grows | yes | `ToolUse`; child re-read |
| fork sub-agent | `Agent` without type (`subagent_type: fork`; default-on in `-p` since 2.1.302) | child opens with `fork-context-ref` `{parentSessionId, parentLastUuid, contextLength}`; meta `agentType: fork`, `isFork` | yes from 2.1.304 (2.1.296 spawned a plain `general-purpose` child) | `lineage.forked_from`, `extra.claude.fork_context` |
| agent teams | `TeamCreate`/teammates (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`) | — | no: tool not offered headless | — |
| workflow | `Workflow` (saved `.claude/workflows/<name>.js`) | `workflows/wf_<run>.json`, `workflows/scripts/<name>-wf_<run>.js`, `subagents/workflows/wf_<run>/agent-*.jsonl` + `journal.jsonl` | yes (2 agents, 1 phase) | `claude_workflow::workflows`; children via `subagent_tree` with journal results |
| background Bash | `Bash run_in_background` | launch result, output file under `$CLAUDE_CODE_TMPDIR`, `<task-notification>` turn | yes | `ToolResult`; notification as `Prompt`/`harness` |
| Monitor | `Monitor` | — | no: remote flag `tengu_amber_sentinel` | — |
| cron | `CronCreate`/`CronList`/`CronDelete` | tool calls (session-only jobs) | yes | `ToolUse` |
| wakeup | `ScheduleWakeup` | tool call, then (process waits) a `user` turn `isMeta`, `turnOrigin: scheduled` | yes (fired) | `Prompt` with `origin: scheduler` |
| /loop | bundled skill over cron | — | no (cron covered) | — |
| skills | `/corpus-greet` and the `Skill` tool | command rows + skill body as a user text block; `attachment.skill_listing`, `command_permissions` | yes | invocation `Notice`, body `InjectedContext`; slash body `skill/load` (tool-linked bodies deduplicated), listing `InjectedContext` |
| MCP | `--mcp-config` stdio server | `mcp__corpus__add` tool_use | yes | `ToolUse` named `mcp__corpus__add` |
| ToolSearch (deferred tools) | `ToolSearch` | `tool_reference` result blocks, `attachment.deferred_tools_delta`/`deferred_tools_record` | yes (MCP tool loaded through it) | `ToolResult`; deltas `InjectedContext` |
| worktree | `EnterWorktree`/`ExitWorktree` | `worktree-state`, `relocated` | yes | carried; `relocated_cwd` in bag |
| web fetch | `WebFetch` | tool call + result (a second, small-model call summarizes) | yes | `ToolUse`/`ToolResult` |
| web search | `WebSearch` (server tool) | — | no: Anthropic server tool, Z.ai lacks it | — |
| structured output | `--json-schema` | `StructuredOutput` tool call, `attachment.structured_output` | yes | `ToolUse` |
| interrupt | SIGINT mid-tool | "[Request interrupted by user for tool use]" user turn, rejected tool_result | yes | `Prompt` (a harness marker read as a prompt) |
| AskUserQuestion | tool | — | no: interactive only | — |
| PushNotification | tool | — | no: remote flag | — |
| system prompt record | `prompt_snapshot` attachment | system prompt + tool schemas | yes (chat session; `--system-prompt-snapshot off` elsewhere to save space) | `Session::system_prompt` |
| request ledger | every API call (2.1.300+) | `api-request` (params, `base`/`keep` + `tail` blob hashes, cache points, digest), `api-request-shape` (system prompt + tool schemas per `shapeHash`), `api-request-blob` (one message per `hash`) | yes from 2.1.304 (every session) | carried under `complete`; not read |
| provider error + retry | the proxy's injected 500 (`CORPUS-PROVIDER-ERROR`) | `system/api_error` `source: request_retry` per attempt, then a synthetic `assistant` API-error turn; the session resumes normally | yes from 2.1.304 | `Error` notices (`extra.claude.retryAttempt`…); `ApiErrorNotice` |
| turn cap | `--max-turns` | `attachment.max_turns_reached` `{maxTurns, turnCount, exitCommitted}` (never rendered) | yes from 2.1.304 | carried under `complete` |
| isolation latch | interactive sessions (`isolation-latch` `{side}`) | one bookkeeping record, no `version` | no (not written headless) | carried under `complete` |
| cost / usage | `cost-state`, `message.usage` | per turn | yes | `Message::usage`; cost carried |
| prompt history | `history.jsonl` | not written by `-p` | no | — |
| live registry | `sessions/<pid>.json` | removed at exit | no | — |
