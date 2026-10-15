//! The read API every door maps onto: a decoded path plus a raw query string in, a `(status,
//! json)` pair out. `cv serve` puts it behind HTTP (adding the Host/CORS/token model and the two
//! tantivy routes it alone can answer); the desktop app calls it in-process. Neither has a second
//! implementation of a noun, so a consumer cannot tell which door a row came through
//! (docs/INTERFACE-V2.md §3) — the rows here are the same functions `cv <cmd> --json` prints with.
//!
//! No socket or headers. Bounded source-verified read state reuses unchanged transcripts. Every error is a JSON
//! `{"error": …}` with the status that names it (400 caller mistake, 404 absent, 410 removed with a
//! pointer, 500 broke).

use crate::ir::{Harness, SessionRef};
use crate::stream::{Flow, MessageSink};
use crate::{Message, Session};
use serde_json::{json, Value};

mod read;

/// Route a decoded path (`["api", "session", "claude", "abc"]`) plus a raw query string.
/// Never panics; an unknown path is a 404.
pub fn route_json(parts: &[&str], query: &str) -> (u16, Value) {
    let _reads = crate::source::read_scope();
    match parts {
        ["api", "health"] => ok(json!({
            "ok": true,
            "harnesses": Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
        })),

        ["api", "sessions"] => sessions(query),

        ["api", "session", harness, id] => session(harness, id),

        ["api", "session", harness, id, "head"] => session_head(harness, id),

        ["api", "session", harness, id, "messages"] => messages(harness, id, query),

        ["api", "session", harness, id, "events"] => session_events(harness, id, query),

        ["api", "session", harness, id, "compactions"] => compactions(harness, id),

        ["api", "touched"] => touched(query),

        ["api", "session", harness, id, "subagents"] => subagents(harness, id),

        ["api", "session", harness, parent, "subagent", agent] => subagent(harness, parent, agent),

        ["api", "session", harness, id, "workflow", wf, "script"] => workflow_script(harness, id, wf),

        // Removed in 0.15.0 with the coordination board: a pointer, not a silent 404.
        ["api", "board", ..] | ["api", "claims", _] | ["api", "who", _] | ["api", "channels"] => err(
            410,
            "removed in 0.15.0 with the coordination board — the task log is the fleet's record: \
             /api/tasks, /api/tasks/inbox/{who}, /api/task/{id}/events",
        ),

        ["api", "tasks"] => tasks(query),

        // Removed in 0.15.0 with the land facet: a pointer, not a silent 404.
        ["api", "tasks", "debt"] | ["api", "tasks", "stats"] => err(
            410,
            "removed in 0.15.0 with the task land facet — completion is self-reported and labeled; \
             read /api/tasks and /api/task/{id}",
        ),

        ["api", "tasks", "inbox", who] => tasks_inbox(who),

        ["api", "task", id] => task_one(id, Query::parse(query).flag("events")),

        ["api", "task", id, "events"] => task_events(id),

        _ => err(404, "not found"),
    }
}

/// A completed staged response. Its payload stays operation-owned until HTTP publication.
pub struct StagedJson {
    pub status: u16,
    pub body: crate::bytes::ByteSource,
    pub bytes: u64,
}

/// Bounded backend for the HTTP message-window route. Other routes keep `route_json`.
/// Source and serialization failures are converted into a staged error before publication.
pub fn route_staged(parts: &[&str], query: &str) -> anyhow::Result<Option<StagedJson>> {
    let _reads = crate::source::read_scope();
    let ["api", "session", harness, id, "messages"] = parts else {
        return Ok(None);
    };
    let result = message_window(harness, id, query, true);
    let mut output = crate::bytes::ByteStage::default();
    let mut counted = Counted {
        writer: &mut output,
        bytes: 0,
    };
    let status = match result {
        Ok((r, mut sink)) => {
            let frames = sink
                .staged
                .take()
                .expect("staged window destination")
                .into_inner()
                .map_err(|error| error.into_error())?
                .finish()?;
            let envelope = window_envelope(&r, &sink, Value::Array(Vec::new()));
            crate::projection::write_json_array(&mut counted, &envelope, "messages", &frames)?;
            anyhow::ensure!(
                sink.reads.fresh(&mut crate::source::Probe::default()),
                "API window source changed while staging response"
            );
            200
        }
        Err((status, error)) => {
            serde_json::to_writer(&mut counted, &error)?;
            status
        }
    };
    let bytes = counted.bytes;
    Ok(Some(StagedJson {
        status,
        body: output.finish()?,
        bytes,
    }))
}
struct Counted<W> {
    writer: W,
    bytes: u64,
}
impl<W: std::io::Write> std::io::Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.writer.write(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(n as u64)
            .ok_or_else(|| std::io::Error::other("JSON response length overflow"))?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

// --- the task log -------------------------------------------------------------------------------

/// Replay the task log; on success hand the outcome to `f`. Log warnings ride every response so
/// a corrupted coordination log is visible from any consumer.
fn with_tasks(f: impl FnOnce(&crate::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match crate::task::replay() {
        Ok(outcome) => f(&outcome),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/tasks?state=&assignee=&repo=&tag=&all=` — task projections, oldest first.
fn tasks(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    with_tasks(|outcome| {
        let filter = crate::task::TaskFilter {
            state: params.get("state"),
            assignee: params.get("assignee"),
            repo: params.get("repo").map(std::path::PathBuf::from),
            include_terminal: params.flag("all"),
            tag: params.get("tag"),
            // The wire sees the whole store: scoping is the shell's default.
            ..Default::default()
        };
        // An unknown state string is a clear 400 (naming the vocabulary), not a silent [].
        let tasks: Vec<crate::task::TaskRow> = match crate::task::list(&outcome.model, &filter) {
            Ok(tasks) => tasks.into_iter().map(crate::task::TaskRow::full).collect(),
            Err(e) => return err(400, &e),
        };
        ok(json!({ "tasks": tasks, "warnings": outcome.warnings }))
    })
}

/// `GET /api/task/{id}[?events=1]` — one task's full projection, plus its raw history on request.
fn task_one(id: &str, with_events: bool) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match crate::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        let t = &outcome.model.tasks[&id];
        let mut v = json!({
            "task": t,
            "effective_state": crate::task::effective_display(t),
            "warnings": outcome.warnings,
        });
        if with_events {
            let history: Vec<&crate::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
            v["events"] = json!(history);
        }
        ok(v)
    })
}

/// `GET /api/task/{id}/events` — the raw durable history of one task.
fn task_events(id: &str) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match crate::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        let history: Vec<&crate::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
        ok(json!({ "events": history, "warnings": outcome.warnings }))
    })
}

/// `GET /api/tasks/inbox/{who}` — what needs this endpoint.
fn tasks_inbox(who: &str) -> (u16, Value) {
    with_tasks(|outcome| {
        let entries = crate::task::inbox_page(outcome, who, None, None, false, chrono::Utc::now()).items;
        ok(json!({ "inbox": entries, "warnings": outcome.warnings }))
    })
}

// --- sessions -----------------------------------------------------------------------------------

/// `GET /api/sessions?harness=&cwd=&limit=` — the §3 session rows, filtered, newest activity first.
fn sessions(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let harness = match params.get("harness") {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            // Validate up front so a typo is a clear 400 rather than silently empty.
            None => return err(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };
    let cwd = params.get("cwd");
    let limit = match params.unsigned("limit") {
        Ok(limit) => limit,
        Err(error) => return err(400, &error),
    };

    // The fast catalog read (~ms when warm); transparently escalates to a full scan when cold.
    let mut refs = crate::sessions();
    if let Some(h) = harness {
        refs.retain(|r| r.harness == h);
    }
    if let Some(needle) = &cwd {
        refs.retain(|r| {
            r.cwd
                .as_ref()
                .is_some_and(|c| c.to_string_lossy().contains(needle.as_str()))
        });
    }
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka)
    });
    if let Some(n) = limit {
        refs.truncate(n);
    }
    ok(Value::Array(
        refs.iter().map(|r| crate::rows::session_row(r, size_of(r))).collect(),
    ))
}

/// The transcript's on-disk size, or `None` when it has been deleted since discovery.
fn size_of(r: &SessionRef) -> Option<u64> {
    std::fs::metadata(&r.path).ok().map(|m| m.len())
}

/// Look up one session by harness name + id (or unique prefix), with each failure already a reply.
fn lookup(harness: &str, id: &str) -> Result<(SessionRef, Box<dyn crate::Adapter>), (u16, Value)> {
    let h = Harness::parse(harness).ok_or_else(|| err(400, &format!("unknown harness {harness:?}")))?;
    match crate::find(id, Some(h)) {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err(err(404, "session not found")),
        Err(e) => Err(err(500, &e.to_string())),
    }
}

/// `GET /api/session/{harness}/{id}` — the full parsed session (the IR), or 404.
// Both full owned-session doors use the same full-fidelity collector as Adapter::parse,
// but retain its operation receipt until JSON projection finishes and is safe to publish.
fn complete_session_json(adapter: &dyn crate::Adapter, reference: &SessionRef) -> anyhow::Result<Value> {
    let mut sink = crate::stream::CollectSink::default();
    let pass = crate::stream::Pass {
        guard: true,
        ..crate::stream::Pass::RECORD
    };
    let read = crate::stream::stream_counted(adapter, reference, &crate::ParseOptions::full(), &mut sink, pass)?;
    let mut session = read.session;
    session.messages = sink.messages;
    session.materialize()?;
    let output = serde_json::to_value(&session)?;
    read.guard.expect("guarded pass").verify(reference)?;
    Ok(output)
}

fn session(harness: &str, id: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    match complete_session_json(adapter.as_ref(), &r) {
        Ok(value) => ok(value),
        Err(error) => err(500, &format!("session read failed: {error:#}")),
    }
}

/// `GET /api/session/{harness}/{id}/head` — everything a session knows about ITSELF, and no
/// messages: the §3 row plus `model`, `git`, `system_prompt`, `lineage`, `extra` and the exact
/// message `total`.
///
/// Why this exists rather than folding into the windowed read: a window stops streaming once it
/// is full, so a session-level fact recorded LATER in the transcript — Claude writes its
/// `prompt_snapshot` well after the opening turns — is simply not reached, and asking the window
/// to keep scanning would make every transcript open pay for the whole file. One cheap pass that
/// keeps no messages answers it instead.
fn session_head(harness: &str, id: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let head = match read::head(read::shared(), adapter.as_ref(), &r) {
        Ok(head) => head,
        Err(e) => return err(500, &format!("stream failed: {e:#}")),
    };
    let mut row = crate::rows::session_row(&r, size_of(&r));
    let map = row.as_object_mut().expect("session_row is an object");
    let Value::Object(head) = head else {
        unreachable!("head metadata is an object")
    };
    for (key, value) in head {
        // Parsed title/cwd replace catalog hints only when present, as before.
        if matches!(key.as_str(), "title" | "cwd") && value.is_null() {
            continue;
        }
        map.insert(key, value);
    }
    ok(row)
}

/// Collects the message window `[start, end)` as serialized IR, never holding more than the
/// window: out-of-window messages pass by as unmaterialized lazy handles, and the stream stops
/// at `end` (after noting whether a message exists there — the `has_more` probe).
struct WindowSink {
    resolver: crate::Resolver,
    primary: std::path::PathBuf,
    /// Index of the next message to arrive.
    idx: usize,
    start: usize,
    /// Exclusive window end; `None` streams to EOF.
    end: Option<usize>,
    msgs: Vec<Value>,
    staged: Option<std::io::BufWriter<crate::bytes::ByteStage>>,
    selected: usize,
    /// A message at `end` exists (so the window isn't the tail of the session).
    more: bool,
    /// Session metadata the stream delivered (codex meta / the parse-bridge), when any did —
    /// claude's native stream emits none.
    meta: Option<Value>,
    fail: Option<anyhow::Error>,
    capture: bool,
    checkpoint: Option<(usize, crate::stream::Checkpoint, Option<Value>)>,
    reads: crate::source::ReadSet,
}

/// The `session` object a windowed read returns. Whatever a session knows about itself has to
/// arrive here or it never reaches a reader: the windowed path is the one every transcript open
/// uses.
fn session_meta_json(s: &Session) -> Value {
    let mut meta = json!({
        "title": s.title,
        "model": s.model,
        "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
        "system_prompt": s.system_prompt,
        "lineage": s.lineage,
        "git": s.git,
    });
    if let Some(principal) = &s.principal {
        meta["principal"] = json!(principal);
    }
    meta
}

impl MessageSink for WindowSink {
    fn dependency(&mut self, path: &std::path::Path) {
        self.reads.record(path);
    }
    fn wants_checkpoint(&mut self, _: u64) -> bool {
        self.capture && self.end.is_some_and(|end| end > self.start && self.idx == end)
    }
    fn checkpoint(&mut self, checkpoint: &mut crate::stream::Checkpoint) {
        self.capture = false;
        if read::bounded_size(&(&*checkpoint, &self.meta)).is_some() {
            self.checkpoint = Some((self.idx, checkpoint.clone(), self.meta.clone()));
        }
    }

    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.fail.take()
    }

    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(session_meta_json(s));
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        read::note_message_sources(&m, &self.primary, &mut self.reads);
        let idx = self.idx;
        self.idx += 1;
        if idx < self.start {
            return Flow::Continue; // before the window: skip without touching content bytes
        }
        if self.end.is_some_and(|e| idx >= e) {
            self.more = true;
            return Flow::Stop; // at the window's end: note the existence, read no further
        }
        if let Some(stage) = self.staged.as_mut() {
            use std::io::Write;
            let result = (|| -> anyhow::Result<()> {
                if self.selected > 0 {
                    stage.write_all(b",")?;
                }
                m.write_json(stage, &self.resolver)?;
                Ok(())
            })();
            return match result {
                Ok(()) => {
                    self.selected += 1;
                    Flow::Continue
                }
                Err(error) => {
                    self.fail = Some(error);
                    Flow::Stop
                }
            };
        }
        let mut m = m;
        if let Err(error) = m.materialize(&self.resolver) {
            self.fail = Some(error);
            return Flow::Stop;
        }
        match serde_json::to_value(&m) {
            Ok(v) => {
                self.msgs.push(v);
                self.selected += 1;
                Flow::Continue
            }
            Err(e) => {
                self.fail = Some(e.into());
                Flow::Stop
            }
        }
    }
}

/// `GET /api/session/{harness}/{id}/messages?start=&end=&extra=` — the window `[start, end)` of the
/// session's messages as IR JSON, without parsing the rest of a huge transcript.
///
/// A lazy stream that skips the messages before `start` without materializing them and **stops
/// at `end`** (the `cv show --range` pattern), so the tail is never read.
/// The reply carries the window's actual indices, `total_known` (+ `total` when the stream
/// reached EOF so the count is exact), and `has_more`.
///
/// `extra=1` additionally keeps each message's harness `extra` map (e.g. `compactMetadata` on a
/// compaction boundary) and the structured tool-result `details` sidecar — the structure view
/// pages through with this on to find the seams.
fn message_window(
    harness: &str,
    id: &str,
    query: &str,
    staged: bool,
) -> Result<(SessionRef, WindowSink), (u16, Value)> {
    let params = Query::parse(query);
    // Window bounds must parse cleanly — a typo'd index is a clear 400, not a silent full read.
    let idx_param = |key: &str| params.unsigned(key).map_err(|error| err(400, &error));
    let start = idx_param("start")?.unwrap_or(0);
    let end = idx_param("end")?;
    if end.is_some_and(|e| e < start) {
        return Err(err(400, "end must be >= start"));
    }
    let want_extra = params.flag("extra");

    let (r, adapter) = lookup(harness, id)?;

    let (shell, mut sink) =
        match read::window_output(read::shared(), adapter.as_ref(), &r, start, end, want_extra, staged) {
            Ok(value) => value,
            Err(e) => return Err(err(500, &format!("stream failed: {e:#}"))),
        };
    // Preserve first pushed metadata; readers which never push it use the returned shell.
    if sink.meta.is_none() {
        sink.meta = Some(session_meta_json(&shell));
    }

    Ok((r, sink))
}

fn window_envelope(r: &SessionRef, sink: &WindowSink, messages: Value) -> Value {
    let total_known = !sink.more;
    json!({
        "harness": r.harness.as_str(), "id": r.id, "start": sink.start,
        "end": sink.start + sink.selected, "has_more": sink.more, "total_known": total_known,
        "total": total_known.then_some(sink.idx), "message_count": r.message_count,
        "session": sink.meta, "messages": messages,
    })
}

fn messages(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let (r, mut sink) = match message_window(harness, id, query, false) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let messages = Value::Array(std::mem::take(&mut sink.msgs));
    ok(window_envelope(&r, &sink, messages))
}

/// `GET /api/session/{harness}/{id}/events?kind=` — the session's extracted tool events (file
/// edits/reads, commands, errors) from the event catalog, in transcript order, as `cv events
/// --json` rows. A stale or missing catalog row is (re)ingested on the spot.
fn session_events(harness: &str, id: &str, query: &str) -> (u16, Value) {
    use crate::events;
    let kind = Query::parse(query).get("kind");
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    if events::needs_ingest(&r) {
        if let Err(e) = events::ingest_ref(&r) {
            return err(500, &format!("event ingest failed: {e:#}"));
        }
    }
    let out: Vec<Value> = events::events_for(r.harness.as_str(), &r.id, kind.as_deref())
        .iter()
        .map(crate::rows::event_row)
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}/compactions` — every compaction boundary in the session, in
/// transcript order, as `cv show --kind compaction_boundary --json` rows plus a `headline` for listings.
///
/// Built on [`crate::compaction::detect`], which streams the whole transcript (memory-safe on a
/// multi-GB session) and pairs each `compact_boundary` with the `isCompactSummary` message that
/// seeds the next window, keeping that **summary text** — how a continued agent recovers the
/// context it can no longer see. `pre_compaction_span` is the `[start, boundary)` range that was
/// compacted away, in the same numbering `/messages` uses.
fn compactions(harness: &str, id: &str) -> (u16, Value) {
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let boundaries = match crate::compaction::detect(&r, true) {
        Ok(b) => b,
        Err(e) => return err(500, &format!("compaction scan failed: {e:#}")),
    };
    let out: Vec<Value> = (0..boundaries.len())
        .map(|n| {
            let mut v = crate::compaction::row(&boundaries, n);
            if let Some(obj) = v.as_object_mut() {
                // An addition for the dashboard, never a rename: the CLI's keys stay the CLI's.
                obj.insert("headline".into(), json!(boundaries[n].headline(n + 1)));
            }
            v
        })
        .collect();
    // A bare array, like every other list this API serves.
    ok(json!(out))
}

/// `GET /api/touched?path=&edits_only=` — sessions whose tool events touched a file (exact or
/// suffix path match), newest activity first, as `cv touched --json` rows. Empty on a cold catalog.
fn touched(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let Some(path) = params.get("path") else {
        return err(400, "missing path parameter");
    };
    let out: Vec<Value> = crate::events::sessions_touching(&path, params.flag("edits_only"))
        .iter()
        .map(crate::rows::touched_row)
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}/subagents` — the sub-agent **forest** a session spawned
/// (directly-spawned `Agent`/`Task` agents and the workflow tier one level deeper), each a §3
/// session row plus the sidecar enrichment the forest view groups by. Empty array if none.
fn subagents(harness: &str, id: &str) -> (u16, Value) {
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let out: Vec<Value> = crate::subagent_tree_of(&r)
        .iter()
        .map(|s| {
            let mut row = crate::rows::session_row(&s.session, size_of(&s.session));
            let map = row.as_object_mut().expect("session_row is an object");
            map.insert("agent_id".into(), json!(s.agent_id()));
            map.insert("agent_type".into(), json!(s.agent_type));
            map.insert("description".into(), json!(s.description));
            map.insert("tool_use_id".into(), json!(s.tool_use_id));
            map.insert("workflow".into(), json!(s.workflow));
            map.insert("result_status".into(), json!(s.result_status));
            map.insert("result_summary".into(), json!(s.result_summary));
            row
        })
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{parent}/subagent/{agent}` — the full parsed transcript of one
/// sub-agent (sub-agents aren't in the main pool, so they're loaded relative to their parent);
/// `agent` may be the full transcript id or the bare `agentId` the journal uses.
fn subagent(harness: &str, parent: &str, agent: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, parent) {
        Ok(x) => x,
        Err((404, _)) => return err(404, "parent session not found"),
        Err(e) => return e,
    };
    let Some(sr) = crate::subagent_tree_filtered_of(&r, crate::SubagentDemand::Sessions, None, |id, _| {
        id == agent || id.strip_prefix("agent-") == Some(agent)
    })
    .into_iter()
    .find(|s| s.session.id == agent || s.agent_id() == agent)
    .map(|s| s.session) else {
        return err(404, "subagent not found");
    };
    match complete_session_json(adapter.as_ref(), &sr) {
        Ok(value) => ok(value),
        Err(error) => err(500, &format!("session read failed: {error:#}")),
    }
}

/// `GET /api/session/{harness}/{id}/workflow/{wf}/script` — the driving script Claude Code recorded
/// for a workflow run, as `{ workflow, name, source }`. Scripts live in the session's sidecar tree
/// at `<session>/workflows/scripts/<slug>-<wf>.js`; the run id (`wf_…`) is matched as the filename
/// suffix (the slug prefix varies per workflow). 404 if there's no script for that run.
fn workflow_script(harness: &str, id: &str, wf: &str) -> (u16, Value) {
    // Reject a path-y run id up front — it's only ever a `wf_…` token, never a path component.
    if wf.contains('/') || wf.contains("..") || wf.is_empty() {
        return err(400, "invalid workflow id");
    }
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let Some(scripts_dir) = crate::harness::claude::sidecar::workflows_dir(&r.path).map(|d| d.join("scripts")) else {
        return err(404, "session has no sidecar tree");
    };
    let found = std::fs::read_dir(&scripts_dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("js"))
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == wf || s.ends_with(&format!("-{wf}")))
            })
    });
    let Some(path) = found else {
        return err(404, "no script recorded for that workflow run");
    };
    match std::fs::read_to_string(&path) {
        Ok(source) => ok(json!({
            "workflow": wf,
            "name": path.file_name().and_then(|n| n.to_str()),
            "source": source,
        })),
        Err(e) => err(500, &format!("could not read script: {e}")),
    }
}

// --- small helpers ------------------------------------------------------------------------------

/// A trivially-parsed query string: `k=v&k2=v2`, percent-decoded, `+` as space.
pub struct Query(Vec<(String, String)>);

impl Query {
    pub fn parse(q: &str) -> Self {
        Query(
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    (decode(&k.replace('+', " ")), decode(&v.replace('+', " ")))
                })
                .collect(),
        )
    }

    /// First non-empty value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    }

    /// A present numeric selector must be nonempty and valid; malformed limits must never
    /// silently expand into an unbounded read. Other string/flag parameters retain their defaults.
    pub fn unsigned(&self, key: &str) -> Result<Option<usize>, String> {
        let Some((_, value)) = self.0.iter().find(|(name, _)| name == key) else {
            return Ok(None);
        };
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(value) = value.parse() {
                return Ok(Some(value));
            }
        }
        Err(format!("{key} must be a non-negative integer, got {value:?}"))
    }

    /// A boolean parameter: present with any value but `0`/`false` (so `?all=1`, `?all=true` and a
    /// bare `?all` all count).
    pub fn flag(&self, key: &str) -> bool {
        self.0
            .iter()
            .any(|(k, v)| k == key && v != "0" && !v.eq_ignore_ascii_case("false"))
    }
}

/// Percent-decode one URL component (a path segment or a query value), lossily for bad UTF-8.
pub fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParseOptions;

    fn window(start: usize, end: Option<usize>) -> WindowSink {
        WindowSink {
            resolver: crate::Resolver::new(None),
            primary: std::path::PathBuf::new(),
            staged: None,
            selected: 0,
            idx: 0,
            start,
            end,
            msgs: Vec::new(),
            more: false,
            meta: None,
            fail: None,
            capture: false,
            checkpoint: None,
            reads: crate::source::ReadSet::default(),
        }
    }

    #[test]
    fn complete_session_json_retains_full_owned_parity_and_rejects_source_changes() {
        let world = crate::test_world("owned-api-source-guard");
        let path = world.write_session(
            "full",
            &[json!({"type":"user", "uuid":"u", "message":{"role":"user", "content":"owned text"}})],
        );
        let reference = SessionRef::new("full", Harness::Claude, &path);
        let adapter = crate::harness::for_harness(Harness::Claude).unwrap();
        assert_eq!(
            complete_session_json(adapter.as_ref(), &reference).unwrap(),
            serde_json::to_value(adapter.parse(&reference).unwrap()).unwrap()
        );
        struct Changes(std::path::PathBuf);
        impl crate::Adapter for Changes {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(
                &self,
                reference: &SessionRef,
                _: &crate::ParseOptions,
                sink: &mut dyn crate::MessageSink,
            ) -> anyhow::Result<Session> {
                sink.dependency(&self.0);
                let mut message = Message::new(crate::Role::User);
                message.content.push(crate::Block::Text {
                    text: "unpublished".into(),
                });
                sink.try_message(message)?;
                std::fs::write(&self.0, "changed while reading")?;
                Ok(Session::new(&reference.id, reference.harness))
            }
        }
        let auxiliary = world.base.join("metadata");
        for dependency in [&path, &auxiliary] {
            std::fs::write(dependency, "before").unwrap();
            let error = complete_session_json(&Changes(dependency.clone()), &reference).unwrap_err();
            assert!(format!("{error:#}").contains("source changed"), "{error:#}");
        }
    }

    #[test]
    fn staged_message_windows_match_value_response_bytes() {
        let world = crate::test_world("api-staged-windows");
        let body = "月🌙é\n\"\\".repeat(4000);
        world.write_session("staged",&[
            json!({"type":"assistant","sessionId":"staged","uuid":"a","message":{"role":"assistant","content":[{"type":"text","text":body},{"type":"tool_use","id":"c","name":"tool","input":{"z":body,"a":2}}]}}),
            json!({"type":"user","sessionId":"staged","uuid":"b","message":{"role":"user","content":"second"}}),
            json!({"type":"assistant","sessionId":"staged","uuid":"c","message":{"role":"assistant","content":"third"}}),
        ]);
        let parts = ["api", "session", "claude", "staged", "messages"];
        for query in [
            "start=0&end=1",
            "start=1&end=2",
            "start=2&end=3",
            "start=1",
            "start=0&end=0",
            "start=9&end=10",
            "extra=1&start=0&end=1",
            "start=oops",
            "start=3&end=1",
        ] {
            let (status, expected) = route_json(&parts, query);
            let actual = route_staged(&parts, query).unwrap().unwrap();
            let mut bytes = Vec::new();
            actual.body.copy_to(&mut bytes).unwrap();
            assert_eq!(actual.status, status, "{query}");
            assert_eq!(actual.bytes, bytes.len() as u64);
            assert_eq!(bytes, serde_json::to_vec(&expected).unwrap(), "{query}");
        }
    }

    #[test]
    fn selected_json_window_reports_read_errors_and_skipped_values_stay_lazy() {
        let world = crate::test_world("api_json_failure");
        let path = world.home.join("input.json");
        let body = r#"{"command":"git status"}"#;
        std::fs::write(&path, body).unwrap();
        let input = crate::json::Json::source(&path, 0, body.len() as u64).unwrap();
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(crate::Block::ToolUse {
            id: "call".into(),
            name: "Bash".into(),
            namespace: None,
            input,
        });
        std::fs::remove_file(path).unwrap();
        let mut selected = window(0, Some(1));
        assert!(crate::stream::checked(&mut selected, |sink| {
            sink.message(message.clone());
            Ok(())
        })
        .is_err());
        assert!(selected.msgs.is_empty());
        let mut before = window(1, Some(2));
        assert_eq!(before.message(message.clone()), Flow::Continue);
        assert!(before.take_error().is_none());
        let mut after = window(0, Some(0));
        assert_eq!(after.message(message), Flow::Stop);
        assert!(after.more);
        assert!(after.take_error().is_none());
    }

    #[test]
    #[ignore = "read-work baseline for pagination design; writes only an isolated scratch fixture"]
    fn pagination_prefix_replay_read_work() {
        use crate::Adapter as _;
        use std::io::Write as _;
        let world = crate::test_world("api_page_read_work");
        let path = world.transcript("pages");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let records = 20_000usize;
        let page = 200usize;
        for i in 0..records {
            writeln!(
                file,
                r#"{{"type":"user","uuid":"m{i}","message":{{"role":"user","content":"hello"}}}}"#
            )
            .unwrap();
        }
        file.flush().unwrap();
        drop(file);
        let r = SessionRef::new("pages", crate::Harness::Claude, path);
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let mut callbacks = 0usize;
        for start in (0..records).step_by(page) {
            let mut sink = window(start, Some(start + page));
            adapter.try_stream(&r, &ParseOptions::lazy(), &mut sink).unwrap();
            assert_eq!(sink.msgs.len(), page);
            assert_eq!(sink.more, start + page < records);
            callbacks += sink.idx;
        }
        let expected = page * (records / page) * (records / page + 1) / 2 + records / page - 1;
        assert_eq!(callbacks, expected);
        let mut head_callbacks = 0usize;
        adapter
            .try_stream(&r, &ParseOptions::lazy(), &mut |_m| {
                head_callbacks += 1;
                Flow::Continue
            })
            .unwrap();
        assert_eq!(head_callbacks, records);
        eprintln!(
            "pages={} selected={} page_callbacks={} additional_head_callbacks={}",
            records / page,
            records,
            callbacks,
            head_callbacks
        );
    }

    #[test]
    fn query_decodes_plus_percent_and_flags() {
        let q = Query::parse("who=ember&since=2h&x=a+b%21&all=1&off=0&bare");
        assert_eq!(q.get("who").as_deref(), Some("ember"));
        assert_eq!(q.get("x").as_deref(), Some("a b!"));
        assert_eq!(q.get("missing"), None);
        assert!(q.flag("all"));
        assert!(!q.flag("off"));
        assert!(!q.flag("missing"));
        // A bare key is an empty value: not a `get` hit, but a flag.
        assert_eq!(q.get("bare"), None);
        assert!(q.flag("bare"));
    }

    #[test]
    fn decode_handles_segments() {
        assert_eq!(decode("%2Fwork%2Fproj"), "/work/proj");
        assert_eq!(decode("plain"), "plain");
    }

    #[test]
    fn removed_routes_point_not_404() {
        for parts in [
            &["api", "board", "fleet"][..],
            &["api", "channels"],
            &["api", "tasks", "debt"],
        ] {
            let (status, v) = route_json(parts, "");
            assert_eq!(status, 410, "{parts:?}");
            assert!(v["error"].as_str().unwrap().contains("removed in 0.15.0"), "{v}");
        }
        assert_eq!(route_json(&["api", "nope"], "").0, 404);
        assert_eq!(route_json(&["api", "sessions"], "harness=warpdrive").0, 400);
    }
}

#[cfg(test)]
mod principal_tests {
    use super::*;

    #[test]
    fn window_metadata_preserves_principal_only_when_present() {
        let mut session = Session::new("opaque", crate::Harness::OpenSession);
        assert!(session_meta_json(&session).get("principal").is_none());
        session.principal = Some(crate::Principal::String("opaque-owner".into()));
        assert_eq!(session_meta_json(&session)["principal"], json!("opaque-owner"));
    }
}
