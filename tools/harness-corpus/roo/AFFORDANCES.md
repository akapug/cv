# Roo Code (CLI 0.1.17 / bundled extension): affordances and where they persist

Per task: `tasks/<uuid>/api_conversation_history.json` (what the model saw), `ui_messages.json` (what
the user saw: `say`/`ask` events), `history_item.json` (the task-history row), `task_metadata.json`
(files in context), `checkpoints/` (shadow git).

| affordance | tool / record | persists? | in 0.1.17 corpus | cv IR today |
|---|---|---|---|---|
| read / list / search | `read_file`, `list_files`, `search_files` | api history `tool_use`/`tool_result` | read_file | ToolUse / ToolResult |
| edit / write | `apply_diff`, `write_to_file` | as above, plus `task_metadata.files_in_context` | apply_diff | ToolUse / ToolResult |
| shell | `execute_command`, `read_command_output` | as above, plus ui `command` / `command_output` | yes | ToolUse / ToolResult |
| todo / plan list | `update_todo_list` | tool call; the list itself lives in ui messages | yes | ToolUse `update_todo_list` |
| modes | `switch_mode`, `--mode`, `history_item.mode` | tool call; the task's mode in `history_item.json` | yes | ToolUse; `extra.roo.mode` |
| sub-agents | `new_task` (child task, own dir) | parent: tool call + ui `subtask_result`; child: `history_item.parentTaskId` | yes | child session with `lineage.parent`; matched mixed-turn result is a `subagent_return` detail |
| follow-up question | `ask_followup_question` | tool call + ui `followup` ask + the answer as `user_feedback` | yes | ToolUse, then the answer turn |
| completion | `attempt_completion` | tool call + ui `completion_result` | yes | ToolUse |
| checkpoints | shadow git per task | ui `checkpoint_saved` (commit hash); git repo in `checkpoints/` | yes (repo not copied) | `checkpoint/create` (store, `ui_messages.json`) |
| compaction (condense) | auto-condense; manual "Condense context" in the IDE | api history `isSummary` + `condenseId`, replaced turns `condenseParent`; ui `condense_context` | yes | CompactionBoundary + CompactionSummary; `extra.roo.condensed_into` |
| approvals | ui `ask: tool` / `ask: command` (auto-approved here) | ui_messages only | yes (auto) | `approval/ask` (store) |
| MCP | `mcp--<server>--<tool>` | tool call | yes | ToolUse |
| skills | `skill` | tool call | no | ToolUse |
| images | user `image` block (base64 source) | api history | yes | Block::Image |
| reasoning | `{type: reasoning, text, summary}` | api history + ui `reasoning` | yes | Block::Thinking |
| usage / cost | `history_item` totals; ui `api_req_started` per request | per task / per request only | yes | `extra.roo` totals; no per-message Usage |
| model | none per message (the CLI's `-m`) | no | n/a | Session.model unset |
| interrupts | ui `api_req_failed` / cancel | ui only | no | not mapped |
