//! Grok CLI adapter — xAI "Grok Build" CLI, `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/`.
//!
//! Each session is a *directory*. The files we read:
//! - `summary.json` — metadata (`info{id,cwd}`, timestamps, model, git, `session_kind`,
//!   `chat_format_version`, `generated_title`/`session_summary`).
//! - `chat_history.jsonl` — the canonical transcript. Per line `type` ∈
//!   `system | user | assistant | tool_result | reasoning`:
//!     * `system`/`user`: `content` is a string (system) or `[{type:text,text}]` (user). User lines
//!       may carry `synthetic_reason` (e.g. `project_instructions`) for injected context.
//!     * `assistant`: `content` (string), optional `reasoning{text,encrypted,id}`, `model_id`,
//!       `model_fingerprint`, and **`tool_calls[]`** (`{id,name,arguments(JSON string)}`).
//!     * `tool_result`: `{tool_call_id, content}` — the output fed back to the model.
//!     * `reasoning` (1.0.5x): `{id, summary[{type:summary_text,text}], encrypted_content?}`, written
//!       before the assistant line it belongs to; it becomes that message's leading `Thinking` block.
//! - `updates.jsonl` — an ACP-style `session/update` event stream (plus `_x.ai/session/update`).
//!   We mine `tool_call`/`tool_call_update` entries (keyed by `toolCallId`) to *enrich* tool calls
//!   with a human `title`, a `kind`, file `locations`, and a `completed`/`failed` status that lets us
//!   set `ToolResult.is_error` correctly (chat_history alone doesn't mark errors). Its
//!   `hook_execution` updates are the session's hook runs (`hook`/`run` affordance rows).
//! - `events.jsonl` — timing / permission lifecycle (`tool_started`, `permission_*`,
//!   `tool_completed{outcome,duration_ms}`, `turn_*`). Not needed for content; we don't ingest it.
//!
//! Historical / variant handling (sniffed by content, never by a version field):
//! - Tool data is read from `chat_history.jsonl` (canonical, always present). `updates.jsonl` is
//!   strictly additive enrichment — if it's empty or absent we still get full tool fidelity.
//! - Sessions with no tool activity (`events.jsonl` empty, short `chat_history`) parse fine.
//! - `session_kind: "subagent"` (under `.grok/worktrees/.../subagent-<id>/`) vs primary sessions
//!   are both handled: the raw `session_kind`/`agent_name` ride in `extra["grok"]`, and a subagent's
//!   `agent_name` nickname also becomes [`Lineage::agent_path`]. 1.0.5x links them: a fork's
//!   `summary.json` has `parent_session_id` (→ `lineage.forked_from`), and a parent lists each child
//!   under `subagents/<child id>/meta.json` (→ the child's `lineage.parent`).

pub(crate) mod emit;
mod record;
pub use emit::emit;

use super::store::document::{self, Fields};
use super::store::jsonl;
use super::wire::openai;
use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::lazy::Text;
use crate::stream::{Checkpoint, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Grok {
    root: Option<PathBuf>,
}

impl Grok {
    pub fn detect() -> Self {
        let root = crate::env::home_dir().map(|h| h.join(".grok").join("sessions"));
        Grok {
            root: root.filter(|p| p.exists()),
        }
    }

    /// An adapter over an explicit Grok home (the dir holding `sessions/`, what `$GROK_HOME` names),
    /// instead of `~/.grok` — for the harness corpus and other tests, without touching the environment.
    pub fn with_home(grok_home: &Path) -> Self {
        let root = grok_home.join("sessions");
        Grok {
            root: root.exists().then_some(root),
        }
    }
}

impl Adapter for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.root.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_name() != "summary.json" {
                continue;
            }
            match scan(entry.path()) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", entry.path().display()),
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Grok, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let (s, enrich) = session_header(r);
            sink.meta(&s);
            stream_history_at(&self.resume_source(r), 0, s, &enrich, 0, opts, sink).map(Option::unwrap)
        })
    }

    fn resume_format(&self) -> Option<u32> {
        Some(1)
    }

    fn resume_source(&self, r: &SessionRef) -> PathBuf {
        r.path.join("chat_history.jsonl")
    }

    fn resume(
        &self,
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        from: &Checkpoint,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let Some(state) = from.get::<HistoryCheckpoint>(crate::stream::ADAPTER_LAYER) else {
                return Ok(None);
            };
            let wire = self.resume_source(r);
            if state.wire != wire {
                return Ok(None);
            }
            let (s, enrich) = session_header(r);
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Grok, sink);
            spawns.meta(&s);
            stream_history_at(&wire, from.offset, s, &enrich, state.skipped, opts, &mut spawns)
        })
    }

    /// The todo list `todo_write` keeps in the session directory (`plan.json` `{todos: {id: …}}`),
    /// then the per-prompt restore points of `rewind_points.jsonl`, then every hook run
    /// `updates.jsonl` recorded ([`hook_runs`]).
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        let plan = fs::read_to_string(r.path.join("plan.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                let items = crate::affordance::items_of(&v);
                crate::affordance::plan_state(crate::affordance::Kind::Plan, "plan.json", items)
            });
        plan.into_iter()
            .chain(rewind_points(&r.path))
            .chain(hook_runs(&r.path))
            .collect()
    }
}

/// `rewind_points.jsonl`: one line per prompt, `{prompt_index, created_at, file_snapshots,
/// after_snapshots}` — the files as they were before (and after) that prompt, which `/rewind`
/// puts back. Each line is a checkpoint.
fn rewind_points(dir: &Path) -> Vec<crate::affordance::Affordance> {
    use crate::affordance::{Affordance, Kind};
    let mut out = Vec::new();
    if jsonl::for_each_record(&dir.join("rewind_points.jsonl"), |v| {
        let Some(v) = Fields::parse(v, &["created_at", "file_snapshots", "prompt_index"])? else {
            return Ok(());
        };
        let ts = v.string("created_at")?.as_deref().and_then(parse_ts);
        let mut files = Vec::new();
        if let Some(snapshots) = v.get("file_snapshots") {
            // Original key order; a repeated key keeps its first place.
            let mut seen = std::collections::HashSet::new();
            snapshots.for_each_entry(|key, _| {
                if seen.insert(key.to_owned()) {
                    files.push(key.to_owned());
                }
                Ok(())
            })?;
        }
        let mut summary = match number(v.get("prompt_index"))?.and_then(|n| n.as_u64()) {
            Some(i) => format!("prompt {i}"),
            None => "prompt".into(),
        };
        if !files.is_empty() {
            summary.push_str(&format!(" · {}", files.join(", ")));
        }
        out.push(Affordance::store_event(
            Kind::Checkpoint,
            "create",
            ts,
            "rewind_points.jsonl",
            &summary,
        ));
        Ok(())
    })
    .is_err()
    {
        out.clear();
    }

    out
}

/// A number field, or None for any other value.
fn number(v: Option<JsonView<'_>>) -> Result<Option<serde_json::Number>> {
    Ok(v.map(JsonView::as_number).transpose()?.flatten())
}

/// A string field of an object view, or None.
fn string(v: Option<JsonView<'_>>, key: &str) -> Result<Option<String>> {
    let Some(field) = v.map(|v| v.get(key)).transpose()?.flatten() else {
        return Ok(None);
    };
    Ok(field
        .is_string()?
        .then(|| field.resolve())
        .transpose()?
        .and_then(|v| v.as_str().map(str::to_owned)))
}

/// The `hook_execution` updates, reduced to metadata. Unknown update/run payloads are validated
/// at the line boundary but never materialized.
fn hook_runs(dir: &Path) -> Vec<crate::affordance::Affordance> {
    use crate::affordance::{Affordance, Kind};
    let mut out = Vec::new();
    if jsonl::for_each_record(&dir.join("updates.jsonl"), |v| {
        let Some(params) = v.get("params")? else { return Ok(()) };
        let Some(u) = params.get("update")? else { return Ok(()) };
        if string(Some(u), "sessionUpdate")?.as_deref() != Some("hook_execution") {
            return Ok(());
        }
        let event = string(Some(u), "event_name")?.unwrap_or_else(|| "hook".into());
        let millis = number(
            params
                .get("_meta")?
                .map(|v| v.get("agentTimestampMs"))
                .transpose()?
                .flatten(),
        )?
        .and_then(|n| n.as_i64());
        let timestamp = millis.and_then(chrono::DateTime::from_timestamp_millis);
        let timestamp = if timestamp.is_some() {
            timestamp
        } else {
            number(v.get("timestamp")?)?
                .and_then(|n| n.as_i64())
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        };
        if let Some(runs) = u.get("runs")?.map(|v| v.array()).transpose()?.flatten() {
            for run in runs {
                let run = run?;
                let name = string(Some(run), "name")?.unwrap_or_default();
                let status = run.get("status")?;
                let label = string(status, "status")?.unwrap_or_default();
                let mut summary = format!("{event}: {name} · {label}");
                if let Some(ms) =
                    number(status.map(|v| v.get("elapsed_ms")).transpose()?.flatten())?.and_then(|n| n.as_u64())
                {
                    summary.push_str(&format!(" · {ms} ms"));
                }
                out.push(Affordance::store_event(
                    Kind::Hook,
                    "run",
                    timestamp,
                    "updates.jsonl",
                    &summary,
                ));
            }
        }
        Ok(())
    })
    .is_err()
    {
        out.clear();
    }

    out
}

/// Static metadata/enrichment shared by whole and suffix passes. Mutable sidecars are verified
/// by the consumer's source baseline; an enrichment edit can revise an earlier visible tool.
fn session_header(r: &SessionRef) -> (Session, HashMap<String, ToolEnrich>) {
    // r.path is the session directory. `summary.json` (small) carries all session-level
    // metadata; the transcript is `chat_history.jsonl`, one record per line — streamed.
    let dir = &r.path;
    let summary: Value = fs::read_to_string(dir.join("summary.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);

    let mut s = Session {
        source_stamp: None,
        id: r.id.clone(),
        harness: Harness::Grok,
        cwd: summary
            .pointer("/info/cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title: r.title.clone(),
        created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
        updated_at: summary
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
        model: summary
            .get("current_model_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        git: grok_git(&summary),
        messages: Vec::new(),
        source_path: Some(dir.clone()),
        extra: serde_json::Map::new(),
        principal: None,
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };

    // Sidecar enrichment: tool_call / tool_call_update entries from the ACP update stream.
    let enrich = read_update_enrichment(dir);

    // Subagent / agent-name facts + lineage from summary.json (and a sub-agent's parent from the
    // parent's `subagents/<id>/meta.json`).
    grok_session_facts(&mut s, &summary);
    grok_lineage(&mut s, &summary, dir);

    // Grok persists the system prompt it sent as `system_prompt.txt` — byte-identical to the
    // leading `system` transcript record. It is the session-level fact, never a message.
    if let Ok(sp) = fs::read_to_string(dir.join("system_prompt.txt")) {
        if !sp.trim().is_empty() {
            s.system_prompt = Some(sp);
        }
    }

    (s, enrich)
}

#[derive(serde::Deserialize)]
struct HistoryCheckpoint {
    wire: PathBuf,
    skipped: u64,
}

/// The one Grok read: records `at..` of `chat_history.jsonl`, each through the record codec. A
/// standalone `reasoning` record is held and joins the next assistant turn. After a finished
/// line that delivered a message (and holds no reasoning) the sink may take a checkpoint with
/// the rejected-line count so far. `Ok(None)`: `at` lies beyond the file (a resume that cannot be).
fn stream_history_at(
    wire: &Path,
    at: u64,
    mut s: Session,
    enrich: &HashMap<String, ToolEnrich>,
    prior_skipped: u64,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    let source = JsonSource::capture(wire).with_context(|| format!("reading {}", wire.display()))?;
    let file = fs::File::open(wire).with_context(|| format!("reading {}", wire.display()))?;
    source.check_metadata(file.metadata()?)?;
    if at > source.len() {
        return Ok(None);
    }
    let spans = opts.spans.then_some(&source);
    let session_model = s.model.clone();
    let mut pending: Option<Message> = None;
    let mut stopped = false;
    let mut skipped = prior_skipped;
    jsonl::for_each_line(&source, &file, at, source.len(), |line| {
        let Some(view) = line.view else {
            skipped += u64::from(!line.blank);
            return Ok(Flow::Continue);
        };
        let (is_reasoning, message) = record::message(view, spans, enrich)?;
        let Some(mut m) = message else {
            return Ok(Flow::Continue);
        };
        if is_reasoning {
            match pending.as_mut() {
                Some(p) => p.content.append(&mut m.content),
                None => pending = Some(m),
            }
            return Ok(Flow::Continue);
        }
        if let Some(mut p) = pending.take() {
            if m.role == Role::Assistant {
                p.content.append(&mut m.content);
                m.content = p.content;
            } else if sink.message(p) == Flow::Stop {
                stopped = true;
                return Ok(Flow::Stop);
            }
        }
        if m.model.is_some() && m.model == session_model {
            m.model = None;
        }
        let flow = sink.message(m);
        stopped = flow == Flow::Stop;
        // Reasoning at EOF is provisional: only a finished record with no held reasoning can
        // finalize the emitted prefix. AffordanceSink also excludes unresolved synchronous spawns.
        if !stopped && line.complete && sink.wants_checkpoint(line.end) {
            #[derive(serde::Serialize)]
            struct State<'a> {
                wire: &'a Path,
                skipped: u64,
            }
            let mut checkpoint = Checkpoint::new(line.end);
            checkpoint.put(crate::stream::ADAPTER_LAYER, &State { wire, skipped });
            sink.checkpoint(&mut checkpoint);
        }
        Ok(flow)
    })?;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    if let (Some(p), false) = (pending, stopped) {
        sink.message(p);
    }
    super::note_skipped_lines(&mut s, skipped);
    Ok(Some(s))
}

/// Subagent nickname + Grok-specific session facts from `summary.json`: a subagent's `agent_name`
/// becomes [`Lineage::agent_path`]; the raw `session_kind` / `agent_name` ride in `extra["grok"]`.
fn grok_session_facts(s: &mut Session, summary: &Value) {
    let kind = summary.get("session_kind").and_then(Value::as_str);
    let agent = summary.get("agent_name").and_then(Value::as_str);
    if kind == Some("subagent") {
        if let Some(a) = agent {
            s.lineage.agent_path = Some(a.to_string());
        }
    }
    if kind.is_some() || agent.is_some() {
        let bag = s.harness_extra_mut(Harness::Grok);
        if let Some(k) = kind {
            bag.insert("session_kind".into(), Value::String(k.to_string()));
        }
        if let Some(a) = agent {
            bag.insert("agent_name".into(), Value::String(a.to_string()));
        }
    }
}

/// Who this session came from. A forked or restored session's `summary.json` names its source in
/// `parent_session_id` (1.0.5x, with `forked_at`). A sub-agent's own summary names no parent; the
/// parent records the child under `<parent>/subagents/<child id>/meta.json` (`parent_session_id`),
/// and the parent sits in the same cwd group, so one directory listing finds it.
fn grok_lineage(s: &mut Session, summary: &Value, dir: &Path) {
    let subagent = summary.get("session_kind").and_then(Value::as_str) == Some("subagent");
    let named = summary
        .get("parent_session_id")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    if let (Some(p), false) = (named, subagent) {
        s.lineage.forked_from = Some(p.to_string());
    }
    if !subagent {
        return;
    }
    let Some(group) = dir.parent() else { return };
    let Ok(rd) = fs::read_dir(group) else { return };
    for e in rd.flatten() {
        let meta = e.path().join("subagents").join(&s.id).join("meta.json");
        let Some(v) = fs::read_to_string(&meta)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        s.lineage.parent = v
            .get("parent_session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| e.file_name().to_str().map(str::to_string));
        s.lineage.spawned_by_tool_use = v.get("tool_call_id").and_then(Value::as_str).map(str::to_string);
        return;
    }
}

fn grok_git(summary: &Value) -> Option<GitInfo> {
    let branch = summary.get("head_branch").and_then(Value::as_str);
    let commit = summary.get("head_commit").and_then(Value::as_str);
    let remote = summary
        .get("git_remotes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    if branch.is_none() && commit.is_none() && remote.is_none() {
        return None;
    }
    Some(GitInfo {
        branch: branch.map(str::to_string),
        commit: commit.map(str::to_string),
        remote: remote.map(str::to_string),
    })
}

/// Enrichment for one tool call, mined from `updates.jsonl`.
#[derive(Default, Clone)]
struct ToolEnrich {
    /// Human-friendly title, e.g. `List \`.\`` (last `tool_call_update.title` wins).
    title: Option<String>,
    /// ACP `kind`, e.g. `read`/`edit`/`execute`/`search`/`other`.
    kind: Option<String>,
    /// File paths the call touched.
    locations: Vec<String>,
    /// Terminal status: `completed` / `failed` (drives `ToolResult.is_error`).
    status: Option<String>,
}

impl ToolEnrich {
    fn is_error(&self) -> bool {
        matches!(self.status.as_deref(), Some("failed") | Some("error"))
    }
}

/// Parse `updates.jsonl` into per-`toolCallId` enrichment. Tolerant: missing/empty/malformed file
/// yields an empty map and zero behavior change.
fn read_update_enrichment(dir: &Path) -> HashMap<String, ToolEnrich> {
    let mut map: HashMap<String, ToolEnrich> = HashMap::new();
    // Preserve first-seen order in the public vector without repeatedly scanning its prefix.
    // Membership belongs to this one read, not the retained enrichment or replay state.
    let mut seen_locations: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    if jsonl::for_each_record(&dir.join("updates.jsonl"), |v| {
        let Some(u) = v.get("params")?.map(|p| p.get("update")).transpose()?.flatten() else {
            return Ok(());
        };
        let Some(u) = Fields::parse(
            u,
            &["sessionUpdate", "toolCallId", "title", "kind", "locations", "status"],
        )?
        else {
            return Ok(());
        };
        let su = u.string("sessionUpdate")?;
        if !matches!(su.as_deref(), Some("tool_call") | Some("tool_call_update")) {
            return Ok(());
        }
        let Some(id) = u.string("toolCallId")? else {
            return Ok(());
        };
        let seen = seen_locations.entry(id.clone()).or_default();
        let e = map.entry(id).or_default();
        if let Some(title) = u.string("title")? {
            if su.as_deref() == Some("tool_call_update") || e.title.is_none() {
                e.title = Some(title)
            }
        }
        if let Some(kind) = u.string("kind")? {
            e.kind = Some(kind)
        }
        if let Some(locations) = u.get("locations").map(|v| v.array()).transpose()?.flatten() {
            for location in locations {
                if let Some(path) = string(Some(location?), "path")? {
                    if seen.insert(path.clone()) {
                        e.locations.push(path)
                    }
                }
            }
        }
        if let Some(status) = u.string("status")? {
            e.status = Some(status)
        }
        Ok(())
    })
    .is_err()
    {
        map.clear();
    }

    map
}

/// `summary_path` is `.../<sid>/summary.json`; the session dir is its parent.
fn scan(summary_path: &Path) -> Result<Option<SessionRef>> {
    let dir = summary_path.parent().map(Path::to_path_buf);
    let Some(dir) = dir else { return Ok(None) };
    let text = fs::read_to_string(summary_path)?;
    let summary: Value = serde_json::from_str(&text)?;

    let id = summary
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_default();

    let cwd = summary.pointer("/info/cwd").and_then(Value::as_str).map(PathBuf::from);

    // title: explicit summary/generated title, else first user message from chat_history.
    let mut title = summary
        .get("session_summary")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            summary
                .get("generated_title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .map(|s| crate::ir::truncate(s, 80));
    // `message_count` is the contract's user + assistant turns (`SessionRef::message_count`), so it
    // is counted from the transcript: summary.json's `num_chat_messages` counts every line —
    // `system` and `tool_result` included — and `num_messages` is the user-turn count. The same
    // pass supplies the title fallback (first non-empty user text).
    let mut message_count = 0usize;
    let initial_title = title.clone();
    if jsonl::for_each_record(&dir.join("chat_history.jsonl"), |v| {
        let ty = string(Some(v), "type")?;
        if matches!(ty.as_deref(), Some("user") | Some("assistant")) {
            message_count += 1;
        }
        if title.is_none() && ty.as_deref() == Some("user") {
            title = super::wire::text_title(v.get("content")?)?;
        }
        Ok(())
    })
    .is_err()
    {
        message_count = 0;
        title = initial_title;
    }

    Ok(Some(SessionRef {
        id,
        harness: Harness::Grok,
        path: dir,
        cwd,
        title,
        created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
        updated_at: summary
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
        message_count,
    }))
}

#[cfg(test)]
mod tests {
    #[derive(Default)]
    struct ResumeKeep {
        messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
    }
    impl MessageSink for ResumeKeep {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.push(message);
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), cp.clone()));
        }
    }

    /// The record codec over an owned value, no spans.
    fn chat_message(v: &Value, enrich: &HashMap<String, ToolEnrich>) -> Option<Message> {
        record::message(v.into(), None, enrich).unwrap().1
    }

    /// A whole stream of `r`, messages collected.
    fn whole(adapter: &Grok, r: &SessionRef, opts: &ParseOptions) -> Session {
        let mut all = ResumeKeep::default();
        let mut s = adapter.try_stream(r, opts, &mut all).unwrap();
        s.messages = all.messages;
        s
    }

    #[test]
    fn enrichment_many_locations_keeps_first_seen_order_per_call() {
        let world = crate::test_world("grok-many-tool-locations");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir).unwrap();
        let mut rows = Vec::new();
        for i in 0..4096 {
            for id in ["a", "b"] {
                rows.push(serde_json::json!({"params":{"update":{
                    "sessionUpdate":"tool_call_update","toolCallId":id,
                    "title":format!("title-{i}"),"status":"completed",
                    "locations":[{"path":format!("{id}/file-{i}")},{"path":format!("{id}/file-0")},{"path":format!("{id}/file-{i}")}]
                }}}).to_string());
            }
        }
        fs::write(dir.join("updates.jsonl"), rows.join("\n")).unwrap();
        let actual = read_update_enrichment(&dir);
        for id in ["a", "b"] {
            let expected = (0..4096).map(|i| format!("{id}/file-{i}")).collect::<Vec<_>>();
            assert_eq!(actual[id].locations, expected);
            assert_eq!(actual[id].title.as_deref(), Some("title-4095"));
            assert_eq!(actual[id].status.as_deref(), Some("completed"));
        }
    }

    #[test]
    fn checked_resume_retains_reasoning_arguments_results_and_source_identity() {
        let world = crate::test_world("grok_selected_resume");
        let dir = world.home.join(".grok/session");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chat_history.jsonl");
        let payload = "雪\n\"\\".repeat(20_000);
        let body = format!(
            "{}\nbad suffix\n{}\n{}\n{}\n",
            serde_json::json!({"type":"user","content":"start"}),
            serde_json::json!({"type":"reasoning","summary":[{"text":payload}],"encrypted_content":"opaque"}),
            serde_json::json!({"type":"assistant","content":payload,"tool_calls":[{"id":"call","name":"Read","arguments":serde_json::json!({"payload":payload}).to_string()},{"id":"structured","name":"Read","arguments":{"payload":payload}}]}),
            serde_json::json!({"type":"tool_result","tool_call_id":"call","content":payload})
        );
        fs::write(&path, &body).unwrap();
        let adapter = Grok::with_home(&world.home.join(".grok"));
        let r = SessionRef::new("session", Harness::Grok, &dir);
        let mut full = ResumeKeep::default();
        let shell = adapter.stream(&r, &ParseOptions::full(), &mut full).unwrap();
        let (count, checkpoint) = &full.checkpoints[0];
        let mut resumed = ResumeKeep::default();
        let suffix = adapter
            .resume(&r, &ParseOptions::lazy(), &mut resumed, checkpoint)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(suffix).unwrap(),
            serde_json::to_value(shell).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&resumed.messages).unwrap(),
            serde_json::to_value(&full.messages[*count..]).unwrap()
        );
        assert!(resumed.messages.iter().flat_map(|m| &m.content).any(|b| matches!(
            b,
            Block::Thinking {
                text: Text::Source(_),
                ..
            }
        )));
        assert_eq!(
            resumed.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
            full.checkpoints.iter().skip(1).map(|(_, cp)| cp).collect::<Vec<_>>()
        );
        assert!(resumed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { id, input: crate::Json::Source(_), .. } if id == "structured")));
        // Truncation, not growth: an append-only log may grow under a retained source.
        fs::write(&path, "").unwrap();
        assert!(serde_json::to_value(&resumed.messages).is_err());
    }

    #[test]
    fn resume_corpus_keeps_grouping_enrichment_and_exact_suffixes() {
        let world = crate::test_world("grok_resume_corpus");
        let adapter = Grok::with_home(&world.home.join(".grok"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/grok");
        let mut files = 0;
        let mut resumes = 0;
        for file in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if file.file_name() != "chat_history.jsonl" {
                continue;
            }
            let r = SessionRef::new("corpus", Harness::Grok, file.path().parent().unwrap());
            for opts in [
                ParseOptions::lazy(),
                ParseOptions::lazy_extra(),
                ParseOptions::full(),
                ParseOptions::complete(),
            ] {
                let mut full = ResumeKeep::default();
                let shell = adapter.try_stream(&r, &opts, &mut full).unwrap();
                let stride = (full.checkpoints.len() / 12).max(1);
                for (i, (count, cp)) in full.checkpoints.iter().enumerate().step_by(stride) {
                    let mut rest = ResumeKeep::default();
                    let s = adapter.try_resume(&r, &opts, &mut rest, cp).unwrap().unwrap();
                    assert_eq!(serde_json::to_value(s).unwrap(), serde_json::to_value(&shell).unwrap());
                    assert_eq!(
                        serde_json::to_value(&rest.messages).unwrap(),
                        serde_json::to_value(&full.messages[*count..]).unwrap(),
                        "{} @{} grouped suffix",
                        file.path().display(),
                        cp.offset
                    );
                    assert_eq!(
                        rest.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
                        full.checkpoints[i + 1..].iter().map(|(_, cp)| cp).collect::<Vec<_>>()
                    );
                    resumes += 1;
                }
            }
            files += 1;
        }
        assert!(files >= 15 && resumes >= 100, "native files={files} resumes={resumes}");
        eprintln!("grok native files={files} exact suffix resumes={resumes}");
    }

    #[test]
    fn resume_replays_eof_reasoning_and_partial_records_and_reads_only_appends() {
        use std::io::Write as _;
        let world = crate::test_world("grok_reasoning_resume");
        let dir = world.home.join(".grok/sessions/group/session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"info":{"id":"session","cwd":"/old"},"current_model_id":"grok"}"#,
        )
        .unwrap();
        let line = "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}\n";
        let prefix = format!("{}bad json\n{{\"type\":\"reasoning\",\"summary\":[{{\"text\":\"thinking\"}}]}}\n{{\"type\":\"assistant\",\"model_id\":\"grok\",\"content\":", line.repeat(20_000));
        fs::write(dir.join("chat_history.jsonl"), &prefix).unwrap();
        let mut r = SessionRef::new("session", Harness::Grok, &dir);
        let adapter = Grok::with_home(&world.home.join(".grok"));
        let opts = ParseOptions::lazy();
        let mut first = ResumeKeep::default();
        adapter.try_stream(&r, &opts, &mut first).unwrap();
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        assert_eq!(first.messages.len(), count + 1); // Provisional EOF reasoning is replayed.
        assert!(serde_json::to_vec(cp).unwrap().len() < 512);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(adapter.resume_source(&r))
            .unwrap();
        writeln!(file, "\"answer\"}}").unwrap();
        file.write_all(line.repeat(200).as_bytes()).unwrap();
        drop(file);
        r.title = Some("current catalog title".into());
        let expected = whole(&adapter, &r, &opts);
        let mut suffix = ResumeKeep::default();
        let mut shell = adapter.try_resume(&r, &opts, &mut suffix, cp).unwrap().unwrap();
        assert_eq!(suffix.messages.len(), 201);
        assert_eq!(shell.title, r.title);
        shell.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let mut other = r.clone();
        other.path = dir.join("other");
        let mut none = ResumeKeep::default();
        assert!(adapter.try_resume(&other, &opts, &mut none, cp).unwrap().is_none());
        assert!(none.messages.is_empty());
        // A UTF-8 failure remains a tolerant skipped line rather than poisoning subsequent records.
        let mut bytes = fs::read(adapter.resume_source(&r)).unwrap();
        bytes.extend_from_slice(b"\xff\n");
        bytes.extend_from_slice(line.as_bytes());
        fs::write(adapter.resume_source(&r), bytes).unwrap();
        let after = whole(&adapter, &r, &opts);
        assert_eq!(after.messages.len(), count + 1 + 200 + 1);
        assert_eq!(after.cv_extra().unwrap()["skipped_lines"], 2);
        eprintln!(
            "grok stable_prefix=20000 appended_callbacks=201 whole_callbacks=20201 checkpoint_bytes={}",
            serde_json::to_vec(cp).unwrap().len()
        );
    }

    use super::*;
    use crate::harness::Parse;

    #[test]
    fn discovery_title_and_count_match_value_oracle() {
        let world = cv_testkit::World::new("grok-raw-title");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir).unwrap();
        let summary = dir.join("summary.json");
        fs::write(&summary, r#"{"info":{"id":"session"}}"#).unwrap();
        let bodies = [
            serde_json::json!("雪\n\"\\".repeat(3000)),
            serde_json::json!(format!("{}late", " ".repeat(10000))),
            serde_json::json!("\u{2003}".repeat(10000)),
            serde_json::json!([{"text":""},{"ignored":"payload"},{"text":"雪\n".repeat(3000)},{"text":"last"}]),
            serde_json::json!([{"text":""},{"text":""},{"text":"answer"}]),
            serde_json::json!([{"text":""},{"text":""}]),
            serde_json::json!([{"text":false}]),
        ];
        let mut cases: Vec<String> = bodies
            .into_iter()
            .map(|content| serde_json::json!({"type":"user", "content":content}).to_string())
            .collect();
        let mut parts = vec![serde_json::json!({"text":""}); 4096];
        parts.push(serde_json::json!({"text":"late"}));
        cases.push(serde_json::json!({"type":"user","content":parts}).to_string());
        cases.extend([
            r#"{"type":"user","ty\u0070e":"assistant","content":"ignored user title"}"#.to_string(),
            r#"{"type":"user","content":[{"text":"old","te\u0078t":"last"}]}"#.to_string(),
            r#"{"type":"user","content":"must skip","ignored":"\ud800"}"#.to_string(),
            r#"{"type":"user","content":"must skip","ignored":1e400}"#.to_string(),
        ]);
        for first in cases {
            let text = format!("{first}\n{{\"type\":\"user\",\"content\":\"fallback\"}}\n{{\"type\":\"assistant\",\"content\":\"answer\"}}\n");
            fs::write(dir.join("chat_history.jsonl"), &text).unwrap();
            let mut expected_title = None;
            let mut expected_count = 0;
            super::super::for_each_json_line_str(&text, |v| {
                let ty = v.get("type").and_then(Value::as_str);
                expected_count += usize::from(matches!(ty, Some("user" | "assistant")));
                if expected_title.is_none() && ty == Some("user") {
                    let text = super::super::wire::text_parts(v.get("content"));
                    if !text.trim().is_empty() {
                        expected_title = Some(crate::ir::truncate(&text, 80));
                    }
                }
                Flow::Continue
            });
            let actual = scan(&summary).unwrap().unwrap();
            assert_eq!(actual.message_count, expected_count);
            assert_eq!(
                actual.title,
                expected_title,
                "title oracle case with {} source bytes",
                first.len()
            );
        }
        // A non-UTF-8 line is one rejected record, as in the stream.
        fs::write(
            dir.join("chat_history.jsonl"),
            b"{\"type\":\"user\",\"content\":\"prefix\"}\n\xff",
        )
        .unwrap();
        let actual = scan(&summary).unwrap().unwrap();
        assert_eq!(actual.message_count, 1);
        assert_eq!(actual.title.as_deref(), Some("prefix"));
    }

    #[test]
    fn sidecar_projections_preserve_order_selection_and_strict_acceptance() {
        use crate::affordance::{Affordance, Kind};
        let world = cv_testkit::World::new("grok-raw-sidecars");
        let dir = &world.base;
        let updates = [
            r#"{"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"call","title":"bare","locations":[{"path":"b"},{"path":"a"},{"path":"b"}],"kind":"read"}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call","title":"old","ti\u0074le":"rich","status":"completed","locations":[{"path":"a"},{"path":"c"}]}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call","status":"failed"}},"ignored":"\ud800"}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook_execution","event_name":"after","runs":[{"name":"check","status":{"status":"ok","elapsed_ms":3}},{"name":"second","status":null}]},"_meta":{"agentTimestampMs":1700000000123}},"timestamp":1}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook\u005fexecution","runs":[{"name":"escaped"}]}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook\u005fexecution","runs":[{"name":"selected","status":{"status":"ok"}}]}},"ignored":"hook_execution"}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook_execution","runs":[{"name":"invalid"}]}},"ignored":1e400}"#,
        ].join("\n");
        fs::write(dir.join("updates.jsonl"), &updates).unwrap();
        let e = read_update_enrichment(dir);
        assert_eq!(e.len(), 1);
        assert_eq!(e["call"].title.as_deref(), Some("rich"));
        assert_eq!(e["call"].kind.as_deref(), Some("read"));
        assert_eq!(e["call"].status.as_deref(), Some("completed"));
        assert_eq!(e["call"].locations, ["b", "a", "c"]);
        let ts = chrono::DateTime::from_timestamp_millis(1700000000123);
        let expected = [
            Affordance::store_event(Kind::Hook, "run", ts, "updates.jsonl", "after: check · ok · 3 ms"),
            Affordance::store_event(Kind::Hook, "run", ts, "updates.jsonl", "after: second · "),
            Affordance::store_event(Kind::Hook, "run", None, "updates.jsonl", "hook: escaped · "),
            Affordance::store_event(Kind::Hook, "run", None, "updates.jsonl", "hook: selected · ok"),
        ];
        assert_eq!(
            serde_json::to_value(hook_runs(dir)).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let snapshots = format!(
            r#"{{"prompt_index":2,"file_snapshots":{{"b":{{"body":"{}"}},"a":1,"b":2}},"after_snapshots":{{"ignored":"{}"}}}}"#,
            "x".repeat(65536),
            "x".repeat(65536)
        );
        fs::write(
            dir.join("rewind_points.jsonl"),
            format!("{snapshots}\n{{\"file_snapshots\":null}}\n{{\"file_snapshots\":{{\"bad\":\"\\ud800\"}}}}\n"),
        )
        .unwrap();
        let expected = [
            Affordance::store_event(
                Kind::Checkpoint,
                "create",
                None,
                "rewind_points.jsonl",
                "prompt 2 · b, a",
            ),
            Affordance::store_event(Kind::Checkpoint, "create", None, "rewind_points.jsonl", "prompt"),
        ];
        assert_eq!(
            serde_json::to_value(rewind_points(dir)).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // A non-UTF-8 line is one rejected record; the rows around it stand.
        let mut bad = updates.into_bytes();
        bad.extend_from_slice(b"\n\xff");
        fs::write(dir.join("updates.jsonl"), bad).unwrap();
        assert_eq!(hook_runs(dir).len(), 4);
        assert_eq!(read_update_enrichment(dir).len(), 1);
        fs::write(dir.join("rewind_points.jsonl"), b"{}\n\xff").unwrap();
        assert_eq!(rewind_points(dir).len(), 1);
    }

    fn enrich_from(s: &str) -> HashMap<String, ToolEnrich> {
        let dir = tempdir_with("updates.jsonl", s);
        let e = read_update_enrichment(&dir);
        let _ = fs::remove_dir_all(&dir);
        e
    }

    /// Write `name`->`contents` into a fresh temp dir, return the dir.
    fn tempdir_with(name: &str, contents: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "cv-grok-test-{}-{}",
            std::process::id(),
            // cheap unique-ish suffix
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), contents).unwrap();
        d
    }

    #[test]
    fn assistant_tool_calls_become_tooluse_blocks() {
        let line = r#"{"type":"assistant","content":"","model_id":"grok-build","model_fingerprint":"fp_x","reasoning":{"text":"thinking","encrypted":"BLOB","id":"r1"},"tool_calls":[{"id":"call-1","name":"read_file","arguments":"{\"target_file\":\"a.rs\",\"limit\":10}"}]}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).expect("assistant message");
        assert_eq!(m.role, Role::Assistant);
        // thinking + tool_use (content empty so no text block)
        assert!(matches!(m.content[0], Block::Thinking { .. }));
        match &m.content[1] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "read_file");
                assert_eq!(input.resolve().unwrap()["target_file"], "a.rs");
                assert_eq!(input.resolve().unwrap()["limit"], 10);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(m.kind, MessageKind::Reply);
        assert_eq!(m.origin, Origin::Model);
        let bag = m.harness_extra(Harness::Grok).expect("grok bag");
        assert_eq!(bag["model_fingerprint"], "fp_x");
        assert!(!m.extra.contains_key("model_fingerprint"), "no flat keys");
        // encrypted reasoning preserved
        match &m.content[0] {
            Block::Thinking { encrypted, text, .. } => {
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(text, "thinking");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_result_becomes_tool_role_message() {
        let line = r#"{"type":"tool_result","tool_call_id":"call-1","content":"file contents"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).expect("tool message");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.kind, MessageKind::ToolResult);
        assert_eq!(m.origin, Origin::Harness);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn enrichment_marks_failed_results_as_error_and_adds_metadata() {
        let updates = concat!(
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-9","title":"grep","rawInput":{}}}}"#,
            "\n",
            r#"{"timestamp":2,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-9","kind":"search","title":"Search foo","locations":[{"path":"src/lib.rs"}]}}}"#,
            "\n",
            r#"{"timestamp":3,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-9","status":"failed"}}}"#,
        );
        let enrich = enrich_from(updates);
        let e = enrich.get("call-9").expect("enrichment present");
        assert_eq!(e.title.as_deref(), Some("Search foo"));
        assert_eq!(e.kind.as_deref(), Some("search"));
        assert_eq!(e.locations, vec!["src/lib.rs".to_string()]);
        assert!(e.is_error());

        // tool_result with this id should be flagged as error and carry metadata via the call.
        let res: Value =
            serde_json::from_str(r#"{"type":"tool_result","tool_call_id":"call-9","content":"boom"}"#).unwrap();
        let m = chat_message(&res, &enrich).unwrap();
        match &m.content[0] {
            Block::ToolResult { is_error, .. } => assert!(*is_error),
            _ => unreachable!(),
        }

        // and the ToolUse extra picks up title/kind/locations.
        let call: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-9","name":"grep","arguments":"{}"}]}"#,
        )
        .unwrap();
        let m = chat_message(&call, &enrich).unwrap();
        let tc = &m.harness_extra(Harness::Grok).expect("grok bag")["tool_calls"]["call-9"];
        assert_eq!(tc["title"], "Search foo");
        assert_eq!(tc["kind"], "search");
    }

    #[test]
    fn user_synthetic_reason_preserved() {
        let line =
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"synthetic_reason":"project_instructions"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hi"));
        // A `synthetic_reason` user turn is harness-injected context, not a prompt.
        assert_eq!(m.kind, MessageKind::InjectedContext);
        assert_eq!(m.origin, Origin::Harness);
        assert_eq!(
            m.harness_extra(Harness::Grok).unwrap()["synthetic_reason"],
            "project_instructions"
        );
        assert!(!m.extra.contains_key("synthetic_reason"), "no flat keys");
    }

    /// The kind/origin mapping across every Grok record shape, plus the `<user_info>` env block.
    #[test]
    fn kind_and_origin_by_record() {
        let msg = |line: &str| chat_message(&serde_json::from_str::<Value>(line).unwrap(), &HashMap::new());

        // A real typed prompt.
        let p = msg(r#"{"type":"user","content":[{"type":"text","text":"do the thing"}],"synthetic_reason":null}"#)
            .unwrap();
        assert_eq!((p.kind, p.origin), (MessageKind::Prompt, Origin::Human));

        // The `<user_info>` environment block is harness-injected context, though unflagged.
        // (The newlines inside the block are JSON `\n` escapes: a literal newline inside a JSON
        // string is a control character and would not parse.)
        let ui = msg(
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: mac\n</user_info>"}],"synthetic_reason":null}"#,
        )
        .unwrap();
        assert!(ui.text().is_some_and(|t| t.contains("\nOS: mac\n")));
        assert_eq!((ui.kind, ui.origin), (MessageKind::InjectedContext, Origin::Harness));

        // A compaction summary seeds the next window.
        let cm = msg(
            r#"{"type":"user","content":[{"type":"text","text":"summary…"}],"synthetic_reason":"compaction_meta"}"#,
        )
        .unwrap();
        assert_eq!((cm.kind, cm.origin), (MessageKind::CompactionSummary, Origin::Harness));

        // The system record is the system prompt.
        let sys = msg(r#"{"type":"system","content":"You are Grok."}"#).unwrap();
        assert_eq!(
            (sys.role, sys.kind, sys.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );

        // A model reply.
        let a = msg(r#"{"type":"assistant","content":"hi"}"#).unwrap();
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
    }

    #[test]
    fn reasoning_records_join_the_next_turn_and_lineage_comes_from_summary_and_meta() {
        let group = tempdir_with("x", "");
        let parent = group.join("p1");
        let child = group.join("c1");
        let fork = group.join("f1");
        for d in [&parent, &child, &fork] {
            fs::create_dir_all(d).unwrap();
        }
        fs::create_dir_all(parent.join("subagents/c1")).unwrap();
        fs::write(
            parent.join("subagents/c1/meta.json"),
            r#"{"parent_session_id":"p1","child_session_id":"c1","tool_call_id":"call_7"}"#,
        )
        .unwrap();
        fs::write(
            child.join("summary.json"),
            r#"{"info":{"id":"c1"},"session_kind":"subagent","agent_name":"general-purpose"}"#,
        )
        .unwrap();
        fs::write(
            fork.join("summary.json"),
            r#"{"info":{"id":"f1"},"parent_session_id":"p1","forked_at":"2026-10-11T21:04:37Z"}"#,
        )
        .unwrap();
        // Shapes from the 1.0.50 corpus: reasoning, then the reply; then trailing reasoning alone.
        let chat = [
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}],"prompt_index":0}"#,
            r#"{"type":"reasoning","id":"","summary":[{"type":"summary_text","text":"think"}]}"#,
            r#"{"type":"assistant","content":"hello","model_id":"m"}"#,
            r#"{"type":"reasoning","id":"","summary":[{"type":"summary_text","text":"cut off"}]}"#,
        ]
        .join("\n");
        for d in [&child, &fork] {
            fs::write(d.join("chat_history.jsonl"), &chat).unwrap();
        }
        let grok = Grok {
            root: Some(group.clone()),
        };
        let c = grok
            .parse(&scan(&child.join("summary.json")).unwrap().unwrap())
            .unwrap();
        assert_eq!(c.lineage.parent.as_deref(), Some("p1"));
        assert_eq!(c.lineage.spawned_by_tool_use.as_deref(), Some("call_7"));
        assert_eq!(c.lineage.forked_from, None);
        let f = grok.parse(&scan(&fork.join("summary.json")).unwrap().unwrap()).unwrap();
        assert_eq!(f.lineage.forked_from.as_deref(), Some("p1"));
        assert_eq!(f.lineage.parent, None);
        let kinds: Vec<Vec<&str>> = f
            .messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        Block::Thinking { .. } => "thinking",
                        Block::Text { .. } => "text",
                        _ => "other",
                    })
                    .collect()
            })
            .collect();
        assert_eq!(kinds, vec![vec!["text"], vec!["thinking", "text"], vec!["thinking"]]);
        let _ = fs::remove_dir_all(&group);
    }

    #[test]
    fn compaction_meta_on_the_user_info_block_is_context_not_the_summary() {
        let msg = |line: &str| chat_message(&serde_json::from_str::<Value>(line).unwrap(), &HashMap::new()).unwrap();
        let ui = msg(
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: mac\n</user_info>"}],"synthetic_reason":"compaction_meta"}"#,
        );
        assert_eq!(ui.kind, MessageKind::InjectedContext);
        let sum = msg(
            r#"{"type":"user","content":[{"type":"text","text":"This session is being continued"}],"synthetic_reason":"compaction_meta"}"#,
        );
        assert_eq!(sum.kind, MessageKind::CompactionSummary);
    }

    #[test]
    fn empty_updates_is_harmless() {
        assert!(read_update_enrichment(Path::new("/nonexistent/dir/xyz")).is_empty());
    }

    #[test]
    fn parses_fixture_session_v1() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/v1_tools");
        if !dir.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&dir.join("summary.json")).unwrap().unwrap();
        let grok = Grok { root: None };
        let s = grok.parse(&r).unwrap();
        // The contract count: user + assistant turns (the fixture's `system` line and two
        // `tool_result` lines are not turns; summary.json's `num_chat_messages` says 5).
        assert_eq!(r.message_count, 4);
        assert_eq!(
            r.message_count,
            s.messages
                .iter()
                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                .count(),
            "discover's count is the parse's user+assistant count"
        );
        // Every Grok fact is nested under `extra["grok"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);
        // roles present: system, user, assistant(with tool_use), tool(result)
        assert!(s.messages.iter().any(|m| m.role == Role::System));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        // call-2 (write_file) is marked `failed` in updates.jsonl -> is_error must be true;
        // call-1 (read_file) is `completed` -> false.
        let mut saw_call2 = false;
        for m in s.messages.iter().filter(|m| m.role == Role::Tool) {
            if let Block::ToolResult {
                tool_use_id, is_error, ..
            } = &m.content[0]
            {
                match tool_use_id.as_str() {
                    "call-1" => assert!(!*is_error),
                    "call-2" => {
                        assert!(*is_error, "fixture marks call-2 failed");
                        saw_call2 = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_call2, "expected call-2 tool result");
        assert_eq!(s.title.as_deref(), Some("Add a greeting function"));
        // The system record → SystemPrompt kind; system_prompt.txt (if present) seeds the session.
        assert!(s
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::SystemPrompt && m.role == Role::System));
        // summary.json marks this a subagent session.
        assert_eq!(
            s.harness_extra(Harness::Grok)
                .and_then(|b| b.get("session_kind"))
                .and_then(Value::as_str),
            Some("subagent")
        );
    }
}
