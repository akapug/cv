# Affordances: one vocabulary for what agents do besides talk (2026-10-11)

The ask (ember): "cv should have affordances for richly querying all those things in a way that
feels natural to agents when they are within those harnesses." Plans, schedules, background jobs,
sub-agent spawns, skills, hooks, approvals, checkpoints, branches, forks, model switches: every
harness has them, each
spells them its own way (`TodoWrite`, `update_plan`, `write_todos`, `todo_write`, `todowrite`,
`todo__todo_write`, `SetTodoList`, `Checklist`, `update_todo_list`, `todo` are one thing), and
several keep the state in a side file cv never opened. The evidence is the per-harness
`tools/harness-corpus/<h>/AFFORDANCES.md` tables and the committed corpus.

## The vocabulary

Fourteen words, snake_case, each a value of `cv_core::affordance::Kind`:

| kind | what it is | actions |
|---|---|---|
| `plan` | a todo / task list or plan-mode switch | `write` (whole list), `create`, `update`, `read`, `enter`, `exit`, `state` |
| `goal` | a standing objective the harness tracks | `create`, `update`, `read`, `state` |
| `schedule` | cron jobs, wakeups, loops, heartbeats | `create`, `delete`, `list`, `fire`, `state` |
| `background_job` | a shell or task left running | `start`, `read`, `stop`, `complete` |
| `subagent_spawn` | a sub-agent launched here | `spawn`, `state` (a registry's record of the run) |
| `subagent_return` | a sub-agent's result delivered | `return` |
| `skill` | a skill loaded or managed | `load`, `manage` |
| `hook` | a user hook's output reached the model (or, where no output is kept, ran) | `output`, `run` |
| `question` | the agent asked the person | `ask` |
| `approval` | a permission request and its outcome, or the approval mode the session ran under | `ask`, `deny`, `state` (the mode) |
| `checkpoint` | a restorable point (file snapshot, shadow-git commit, saved chat) and its restore | `create`, `restore`, `state` (a tally) |
| `branch` | rewind / undo / reset / clear: what follows does not continue what precedes | `branch` |
| `fork` | this session was forked from another | `fork` |
| `model_change` | the model (or effort) changed | `change` |

**Checkpoint or branch.** A checkpoint is the restorable *point*: what a restore would put back
(files, or a saved conversation). A restore that cuts the conversation is a `branch`, and stays
one: the IR's `Branch` marker sits where the history stops continuing (Kimi `/undo`, Gemini
`$rewindTo`, Kimi Code `/undo`). `checkpoint/restore` is the restore of a recorded point, read
where the store records which point (OpenCode's `revert{messageID, snapshot}`); a harness that
restores files and cuts history at once shows both rows when it marks both. A rewind with no
recorded point is only a branch; a point never restored is only a checkpoint.

**Approval rows or approval mode.** A row per request needs a store that records the request or
its outcome: Roo's `ui_messages.json` `ask`s, Claude's and OpenCode's refusal results, Codex's
refused MCP call. Where a harness keeps only the mode it ran under (Claude `permissionMode`, Codex
`approval_policy`, Kimi Code `permission.set_mode`, Qwen `approvalMode`, Kimi `state.json`
`approval`, Devin `agent_mode`, OpenCode's session ruleset), that is one session-level
`approval/state` row, never a row per call: in `auto` / `yolo` / `bypass` no call was asked about,
and cv will not invent the asks. No corpus store records a per-call *allow*: Kimi Code's
`permission.record_approval_result` is never written in auto mode, Devin's `tool_call_state`
holds a completion status, not a decision, so neither is read.

## Where it lives: a derived view over the IR, plus three IR corrections

**A view, not new IR.** `cv_core::affordance::of_session(&Session) -> Vec<Affordance>` derives the
rows from what the IR already says, and `Adapter::store_affordances(&SessionRef)` (default: none)
adds the rows that live only in a side store. An `Affordance` is `{kind, action, index?, timestamp?,
tool?, tool_use_id?, summary, items[], source}`; `items` is the normalized list (`{content,
status}`) for `plan`/`goal` state, `source` is `message`, `tool_call` or `store`.

Why not `MessageKind` additions: a plan write is one `ToolUse` inside a `Reply` that may also edit a
file; giving the message a kind would lie about the rest of it, and every emitter would have to
learn twelve kinds that no target store has a record for. Why not a `Block` facet or
`extra["cv"]`: either changes the wire shape of every `show --json` message for a fact cv can
recompute from the tool name. The attractor ("a `--kind`, a `--sort`, a selector, or a route on
the one API") is met by one module, one table, one trait method, no new command.

The mapping by tool name is **one shared table** (`affordance::classify_tool`, harness-scoped where a name
is ambiguous: OpenCode `task` is a sub-agent, Qwen `task_create` is a plan item). Record-shaped
affordances come from the IR's existing kinds and origins: `SubagentSpawn`, `SubagentReturn`,
`Branch`, `ModelChange`, `Origin::Hook`, `Origin::Scheduler` (a `schedule` `fire`), and a short
record table for background completions (Claude `<task-notification>` of a command, Kimi
`<notification>`, Kimi Code origin `task`, Hermes `process_complete`) and Codex's refused MCP call.

**Session-level rows** (`affordance::session_rows`, printed as `session`, no index): what a harness
keeps once per session rather than at a message. The fork, and two bag keys any adapter fills
while it reads past bookkeeping records the lean passes drop: `permission_modes` (the approval
modes, in order of first appearance; `affordance::note_mode`) and `checkpoints` (`{count, files?,
tag?}`; `affordance::note_checkpoints`), plus the facts the IR already carried (Devin
`agent_mode`, OpenCode's `permission` ruleset and `revert`). A tally, not a row per point, because
the records arrive after the message they belong to (a Claude `file-history-snapshot` follows its
prompt) and a streamed pass cannot go back to mark it; one per prompt would also only restate the
prompt list. The catalog's counter adds these rows at `finish`, so a streamed pass and the view
agree.

**Per-message details**: a tool result whose text is the harness's refusal (Claude "Permission to
use … denied" / "doesn't want to proceed", OpenCode's rule refusal and rejection; error results
only) is an `approval/deny` at that message; OpenCode's `patch` parts (1.x) and file-changing
`snapshot{start,end,files}` (2.x) and Devin's `details.restorable` are a `checkpoint/create` there.

**The catalog's counts carry the view's version**: they are the payload of the `counts`
`source::Record`, whose version is the one stored-format version (`affordance::VIEW_VERSION` is
`source::VERSION`). Counts an older view recorded do not decode and read "not counted", so
adding a kind or a reader never makes `has:<kind>` answer a stale no: bump it with a change to
what `Counter` counts, and every consumer rereads once.

**IR corrections** (existing kinds, assigned where adapters left them out; CHANGELOG'd):

1. `SubagentSpawn` / `SubagentReturn` by tool name, through one sink wrapper
   (`affordance::AffordanceSink`, which also does correction 3) the adapter puts around its sink: a `Reply` that calls a spawn tool is
   a `SubagentSpawn`; the `ToolResult` of a synchronous spawn is a `SubagentReturn`/`Subagent`.
   Applied to Claude, OpenCode, Hermes, OpenClaw, Goose, Grok, Roo, Cline, Devin, Continue, Kimi.
   Codex and Kimi Code already mark spawns with their own records and are left alone. A spawn turn
   is still a turn and still model-visible: `Message::is_turn` and the new
   `Message::is_model_visible` count a non-System spawn/return (dataset, distill and lanes switch
   to it), so nothing that used to reach a training row or a lane count disappears.
2. Claude `user` rows the harness wrote are no longer `Prompt`: a `<task-notification>` for an
   agent or workflow is `SubagentReturn`/`Subagent`, for a command `InjectedContext`/`Harness`; an
   interrupt marker (`[Request interrupted by user…]`) and other `isMeta` rows are
   `InjectedContext`/`Harness`. A scheduler fire stays `Prompt`/`Scheduler`.
3. Deferred-tool wrappers are unwrapped the way Qwen already is: Hermes `tool_call{name,
   arguments}` and OpenClaw 10.1 `tool_call{id, args}` become `ToolUse{name: <inner>, namespace:
   "tool_call", input: <arguments>}`, so `tools`, `events`, `tool:` and this table see the real
   tool (the table strips OpenClaw's `openclaw:core:` prefix). Both emitters wrap them back.
4. Devin's `chisel/undo` `restore_file` entries become `ToolResult::details.restorable` (the
   paths `/revert` can put back; the original content stays in the store).

## The mapping (from the evidence)

| kind | tools and records, by harness |
|---|---|
| plan | Claude `TodoWrite` `TaskCreate` `TaskUpdate` `TaskList` `TaskGet` `EnterPlanMode` `ExitPlanMode` + `tasks/<sid>/*.json` (store); Codex `update_plan`; Grok `todo_write` `enter_plan_mode` `exit_plan_mode` + `plan.json` (store); OpenCode `todowrite` `todoread` + `todo` table (store); Gemini `write_todos` `tracker_*` `enter_plan_mode` `exit_plan_mode` + `tracker/*.json`, `plans/*.md` (store); Qwen `todo_write` `task_create` `task_update` `task_list` `exit_plan_mode`; Goose `todo__todo_write` + `extension_data["todo.v0"]` (store); Hermes `todo` `todo_list`; Kimi `SetTodoList` `EnterPlanMode` `ExitPlanMode` + `state.json` `todos` (store); Kimi Code `TodoList` `EnterPlanMode` `ExitPlanMode`; Continue `Checklist`; Roo `update_todo_list`; Devin `todo_write` |
| goal | Kimi Code `CreateGoal` `UpdateGoal` `GetGoal` `SetGoalBudget`; Qwen `get_goal` `update_goal`; Hermes `state_meta` `goal:<sid>` (store) |
| schedule | Claude `CronCreate` `CronList` `CronDelete` `ScheduleWakeup`; Grok `scheduler_create` `scheduler_list` `scheduler_delete`; Kimi Code `CronCreate` `CronList` `CronDelete`; Qwen `cron_create` `cron_list` `cron_delete` `loop_wakeup` + `tmp/<sha256(cwd)>/scheduled_tasks.json` (store); Hermes `cronjob` `cronjob_manage` + `state_meta` `loop:` / `heartbeat:` (store); OpenClaw `cron` `automations` + `cron/jobs.json` / `cron_jobs` (store); Cline `schedule_executions` / `cron_runs` (store fires); fires: every `Origin::Scheduler` turn (Claude wakeups, OpenClaw cron runs, Goose scheduled sessions) |
| background_job | Claude `Bash{run_in_background}` `BashOutput` `KillShell` `Monitor` + command `<task-notification>`; Grok `run_terminal_command{background}` `get_command_or_subagent_output` `monitor` `kill_command_or_subagent`; Gemini/Qwen `run_shell_command{is_background}`; Hermes `terminal{background}` `process` `process_manage` + `process_complete`; OpenClaw `process`; Kimi `TaskOutput` `TaskStop` `TaskList` + `<notification>`; Kimi Code `TaskOutput` `TaskStop` `TaskList` `WaitFor` + origin `task`; Continue `CheckBackgroundJob` |
| subagent_spawn / _return | Claude `Agent` `Task` `Workflow` (+ agent `<task-notification>` returns); OpenCode `task` `subagent`; Gemini `invoke_agent`; Qwen `agent`; Hermes `delegate_task`; OpenClaw `sessions_spawn`; Goose `delegate`; Grok `spawn_subagent`; Kimi `Agent`; Kimi Code `Agent` `AgentSwarm` (records); Roo `new_task`; Cline `spawn_agent` `team_spawn_teammate` + `teams.db` `team_events` (store); OpenClaw `subagents/runs.json` / `subagent_runs` (store); Devin `run_subagent`; Continue `Subagent`; Codex `SubAgentActivity` records |
| skill | Claude `Skill`; OpenCode `skill`; Gemini `activate_skill`; Qwen `skill`; Goose `load_skill`; Hermes `skill_manage`; Kimi Code `Skill`; Roo `skill`; Continue `Skills` |
| hook | every `Origin::Hook` message (Claude attachments and stop summaries, Gemini `<hook_context>`, Qwen prompt-submit context); Grok `updates.jsonl` `hook_execution` (store, `run`) |
| approval | requests: Roo `ui_messages.json` `ask` `tool` / `command` / `use_mcp_server` / `browser_action_launch` (store, `ask`); denials: Claude refusal results (`dontAsk` "Permission to use … denied", rejection "doesn't want to proceed"), OpenCode rule refusal / rejection results, Codex `McpToolCall` "requires approval" notice (`deny`); modes (`state`): Claude `permissionMode` (turns, `permission-mode` records), Codex `approval_policy` (`turn_context`, `thread_settings_applied`), Kimi Code `permission.set_mode`, Qwen `executionContext.approvalMode`, Kimi `state.json` `approval{yolo, afk, auto_approve_actions}`, Devin `sessions.agent_mode`, OpenCode `session.permission` ruleset |
| checkpoint | points (`create`): Roo `checkpoint_saved` (store), Grok `rewind_points.jsonl` (store), OpenCode `patch` parts / `snapshot{files}`, Devin `chisel/undo` `restore_file`; tallies (`state`): Claude `file-history-snapshot` / `file-history-delta`, Kimi Code `file_history.checkpoint`, Kimi `_checkpoint`, Gemini `checkpoint-<tag>.json` (the session is the saved chat); restores (`restore`): OpenCode `session.revert` / `session_v2.revert` |
| question | Claude `AskUserQuestion`; Kimi `AskUserQuestion`; Gemini `ask_user`; Qwen `ask_user_question`; Hermes `clarify`; OpenCode `question`; Roo `ask_followup_question`; Cline `ask_question`; Continue `AskQuestion` |
| branch | every `MessageKind::Branch` (Kimi `/clear`, undo; Kimi Code `/undo`; OpenClaw `reset`; Gemini `$rewindTo`) |
| fork | `Lineage::forked_from` (Codex, Grok, Hermes `/branch`, Kimi Code, Qwen, OpenCode 2.x; Claude `--fork-session` and Goose `--fork` inferred from the records the fork copied) |
| model_change | every `MessageKind::ModelChange` (OpenClaw, OpenCode 2.x, Qwen `/model`) |

## The query surface

- **`cv show <id> --kind plan,schedule,…`**: a third `--kind` view family beside `prompt,answer`
  and the compaction seams. One row per affordance in transcript order (store rows last, and only
  when the store holds something): index, timestamp, kind, action, tool, summary, a plan's items.
  A window narrows the transcript rows. `--kind affordances` is every kind.
  `--json` prints the `Affordance` rows. MCP gets it for free (`show` with `kind`), because the
  roster is the clap tree.
- **`has:<kind>`** in the query calculus: `cv ls -q has:plan`, `cv ls -q 'has:schedule harness:hermes'`.
  The fleet question ("which sessions left a cron job behind") is a selector, not a `lanes` flag;
  `ls`, `stats` and `dataset` take `-q`; MCP `ls` exposes it as `query`. Search uses its own
  full-text query grammar; HTTP session listing exposes harness/cwd filters.
- **Hook body**: the `compact` hook (Claude `SessionStart` after compaction) adds the one command
  that reads back the plan, schedules and background jobs the compacted context held.

## Gaps this closes, and what it leaves

Closes: no `SubagentSpawn` in Claude/OpenCode/Hermes/OpenClaw/Goose (and Grok, Roo, Cline, Devin,
Continue, Kimi); Claude task-notification and interrupt turns typed `Prompt`; `tool_call` hiding
the real tool in Hermes and OpenClaw 10.1; unread side state for Claude `tasks/`, Grok `plan.json`,
OpenCode's `todo` table, Gemini `tracker/` and `plans/`, Goose `extension_data` todo, Hermes
`state_meta` goals, loops and heartbeats; and (the two later kinds) Roo's unread `ui_messages.json`
checkpoints and asks, Grok `rewind_points.jsonl`, and the approval-mode and file-history records
Claude, Codex, Kimi, Kimi Code and Qwen read past.

Then closed (lane `aff-sidefiles`): Qwen `scheduled_tasks.json`; Cline schedule fires and
`teams.db` runs; OpenClaw's cron and sub-agent registries; Kimi `state.json` todos; Grok
`hook_execution`; Claude `--fork-session` and Goose fork lineage, which no store records but the
copied records imply. A side store is read only when a full read rewrites the session's
counts record (its source baseline changed), or for the view itself. Since 0.17 that
baseline is the one `source::Baseline` record every consumer publishes (the catalog's separate
`affordance_sig` and the no-op `note_affordances` are gone), and `AffordanceSink` latches errors
through the one `stream::Latch` the event and spool sinks use.

Then closed in 0.18: Kimi Code `tools.update_store{key:todo,value}` supplies the latest
`plan/state` (rich items, presence only for Facts reducers). Legacy Kimi Code Agent/AgentSwarm
steps infer spawns and completed/failed result headers infer returns; native 2.x markers remain
authoritative and suppress duplicate inference. Roo's mixed user turn stays a `Prompt`: its
matched `new_task` result supplies a `subagent_return` detail. Claude's injected "Base directory
for this skill:" body identifies a slash-loaded skill; `sourceToolUseID` prevents counting a
Skill tool call's body a second time. Qwen's per-project schedule-file identity is already in
its source baseline, including edits, deletion and recreation.

Leaves: a Cline schedule's maker (no owning session is recorded), and OpenClaw 1.29
completions that carry only ordinary user text without a structured completion marker.
Do not infer either from timing or ambiguous prose alone.

For `approval` and `checkpoint`, left: a per-call *allow* (no corpus store records one: Kimi Code
`permission.record_approval_result` is unwritten in auto mode, Devin `tool_call_state` holds only a
status); Claude `/rewind` and Roo's checkpoint restore, which are interactive and in no corpus
store; Qwen `file_history_snapshot` and `session_approval_mode` records, Gemini's shadow-git file
checkpoints, Hermes `checkpoints/` shadow repos, OpenClaw's approvals tables and Goose's
`toolConfirmationRequest`, none of which the corpus persisted; OpenCode's `permission` table (saved
"always allow" rules are per project, not per session); Continue `permissions.yaml` and Cline's
approval mode (configuration only).
