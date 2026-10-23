# Kimi Code: what it persists, and what the corpus covers

Sources: the tool list Kimi Code sends (logged by name in `calls.jsonl`: `Agent`, `AgentSwarm`,
`AskUserQuestion`, `Bash`, `CreateGoal`, `CronCreate`, `CronDelete`, `CronList`, `Edit`,
`EnterPlanMode`, `ExitPlanMode`, `FetchURL`, `GetGoal`, `Glob`, `Grep`, `Read`, `ReadMediaFile`,
`SetGoalBudget`, `Skill`, `TaskList`, `TaskOutput`, `TaskStop`, `TodoList`, `UpdateGoal`, `WaitFor`,
`Write`, plus MCP tools as `mcp__<server>__<tool>`; the same 26 in 0.39.1 and 2.1.1), the bundle's
record types (`dist/main.mjs`), and the stores in `crates/cv-core/tests/corpus/kimi-code/`.
"Record" means a `wire.jsonl` record `type` unless a file is named.

| affordance | persisted as | captured | cv IR |
|---|---|---|---|
| multi-turn chat, resume (`-S`) | `context.append_message` (origin `user`), `turn.prompt` | yes | `prompt`/Human, `reply`/Model |
| read, edit, write, shell, glob | `context.append_loop_event` `tool.call` / `tool.result` | yes (`Grep` not called) | `ToolUse` / `ToolResult` blocks |
| parallel tool calls | several `tool.call` in one step | yes | one `reply` with several `ToolUse` |
| tool error | `tool.result.result.isError` | yes | `ToolResult{is_error}` |
| thinking | `content.part{part:{type:"think"}}` | yes | `Thinking` (empty ones dropped) |
| image input (`ReadMediaFile`) | image part in `tool.result.output` | **no**: the model used `Bash`/`Read` on `dot.png` | `Image` inside the `ToolResult` |
| todo list (`TodoList`) | tool call + `tools.update_store{key,value}` | yes | `ToolUse`; latest todo `plan/state` (presence only under Facts) |
| plan mode (`EnterPlanMode`/`ExitPlanMode`) | `plan_mode.enter/exit`, `plan.revision`, `agents/main/plans/<slug>.md`, `plan/<slug>/v1.md` | yes | `ToolUse`; records carried; plan files not read |
| background shell (`run_in_background`), `TaskOutput` | `task.started`, `task.terminated`, `agents/main/tasks/<id>.json`, `tasks/<id>/output.log`; completion as `context.append_message` origin `task` | yes (`WaitFor`, `TaskStop`, `TaskList` not called) | `notice`/Harness for `task.terminated`; the notification `injected_context`/Scheduler |
| schedules (`CronCreate`/`CronList`/`CronDelete`) | `cron.add`, `cron.delete` | yes; `cron.fired` not (needs a live process at the scheduled time) | `ToolUse`; records carried |
| goals (`CreateGoal`/`UpdateGoal`) | `goal.create`, `goal.update`, `goal.clear` | yes (`GetGoal`, `SetGoalBudget` not called) | `ToolUse`; records carried |
| sub-agent (`Agent`) | `agents/agent-N/wire.jsonl`, `state.json.agents`; 2.x adds `subagent.spawned/started/completed` on the parent | yes | child `Session` (`lineage.parent`, `agent_path`); 2.x `subagent_spawn` / `subagent_return` |
| swarm (`AgentSwarm`) | `swarm_mode.enter/exit`, one `agents/agent-N/` per task; 2.x `swarmIndex` | yes | as sub-agents; `swarmIndex` in `extra["kimi-code"]` |
| skills (`Skill` tool, `~/.kimi-code/skills/`) | tool call + `context.append_message` origin `skill_activation`; 2.x also `prompt.steered` | yes | `ToolUse` + `injected_context`/Harness |
| MCP | `mcp.tools_discovered`, tool `mcp__corpus__add` | yes | `ToolUse` named `mcp__corpus__add` (no namespace split) |
| hooks (`[[hooks]]` in config.toml) | `UserPromptSubmit` stdout as `context.append_message` origin `hook_result`; `PostToolUse` output not persisted | yes | `injected_context`/Harness |
| web fetch (`FetchURL`) | tool call | yes | `ToolUse` |
| web search | no tool in the list (needs the Kimi search service) | n/a | |
| `AskUserQuestion` | would be a tool call; prompt mode's auto permission mode withholds it | **no** (headless) | |
| approvals | `permission.set_mode`; `permission.record_approval_result` never written in auto mode | mode only | carried; `approval/state` |
| compaction | `full_compaction.begin/complete`, `context.apply_compaction{summary, compactedCount}` | yes (auto, small window) | `compaction_boundary` + `compaction_summary` |
| `/undo` | `context.undo{count}`; 2.x adds `agent.switched`, `context.undone` | yes (TUI) | `branch`/Human |
| `/clear` | a new session | yes (TUI) | a separate `Session`, no link |
| interrupt (Esc) | `turn.cancel`; 2.x adds `turn.step.interrupted`, `prompt.aborted` | yes (TUI) | `notice`/Human |
| API error retries | 2.x `turn.step.retrying{errorName, errorMessage}` | yes (TUI, closed port) | carried |
| fork (`kimi fork`, 2.x) | new session, `state.json.forkedFrom`, `forked` record | yes | `lineage.forked_from` |
| file history (2.x) | `file_history.tracked/checkpoint`, `agents/main/file-history/` | yes | carried; `checkpoint/state` tally; snapshots not read |
| memory / project instructions | `AGENTS.md` paths in `profile.bind.agentsMdPaths` | yes | system prompt; paths carried |
| model switch | `config.update{modelAlias}` | **no**: one env-defined model | would be `system_prompt` / model |
| usage | `step.end.usage`, `usage.record` | yes | `Message::usage` |
| plugins | `plugin.session_start` | yes (built-in) | carried |

Not persisted: `CronList` results beyond the tool result, `export` / `vis`, the TUI's input history
(`user-history/`, outside `sessions/`).
