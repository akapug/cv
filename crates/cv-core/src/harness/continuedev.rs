//! Continue adapter — `~/.continue/sessions/` (continue.dev VS Code / JetBrains extension).
//!
//! ## Storage layout
//! Root is `$CONTINUE_GLOBAL_DIR/sessions` or `~/.continue/sessions`. It holds:
//! - **`sessions.json`** — the index: a JSON array of
//!   `{sessionId, title, dateCreated, workspaceDirectory}`. `dateCreated` is a string holding a
//!   millisecond epoch (e.g. `"1700000000000"`) on older builds, or an ISO-8601 timestamp on newer
//!   ones — we accept both. This gives id + title + cwd + created for a listing without opening each
//!   transcript.
//! - **`<sessionId>.json`** — one full session per file:
//!   `{sessionId, title, workspaceDirectory, history: [Item, ...]}`.
//!
//! ## `history[]` item (a `ChatHistoryItem` in Continue)
//! `{message: Message, contextItems: [ContextItem], editorState?, promptLogs?, ...}`. We map one
//! item to one IR `Message`.
//!
//! ### `message` (Continue uses OpenAI-ish `ChatMessage`s)
//! - `role`: `"user" | "assistant" | "system" | "tool"`.
//! - `content`: a **string** OR an array of parts (`{type:"text",text}` /
//!   `{type:"imageUrl",imageUrl:{url}}`). Both shapes are handled.
//! - `toolCalls` (assistant, newer Continue): `[{id, type:"function", function:{name, arguments}}]`.
//!   `arguments` is a JSON-encoded *string* (streamed, so possibly partial) → `Block::ToolUse`.
//! - `toolCallId` (role `tool`): the id of the call this result answers → `Block::ToolResult`.
//! - `usage` (assistant, cn CLI 1.5): OpenAI usage names plus `model` and `cost_cents` → [`Usage`].
//!
//! ### `toolCallStates[]` (assistant items, cn CLI 1.5)
//! The cn CLI writes no `role: "tool"` items; each assistant item carries its calls' outcomes as
//! `{toolCallId, toolCall, status, parsedArgs, output: [{content, name, description}]}`, emitted
//! right after the assistant turn as a Tool message (`status: "errored"` → `is_error`).
//!
//! ### `contextItems[]` (attached files / snippets / docs)
//! `{name, description, content, uri?: {type, value}}`. Best-effort → `Block::File` (path/source from
//! `uri.value` or `description`), appended to the owning message so search can see them.
//!
//! ## Tolerance
//! Missing index → glob `*.json` (minus `sessions.json`). Missing fields → skip. `content` as
//! string or array both work. `dateCreated` ms-epoch or ISO both work. Never panics.

use super::store::document::{Fields, InvalidStruct};
use crate::json::{InvalidJson, JsonSource, JsonView};
mod record;
use super::wire::openai;
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Continue {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
}

impl Continue {
    pub fn detect() -> Self {
        let root = crate::env::var_os("CONTINUE_GLOBAL_DIR")
            .map(PathBuf::from)
            .or_else(|| crate::env::home_dir().map(|h| h.join(".continue")));
        let sessions = root.map(|r| r.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }

    /// Continue's store under an explicit global dir (what `CONTINUE_GLOBAL_DIR` names).
    pub fn with_root(root: &Path) -> Self {
        let sessions = Some(root.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }
}

impl Adapter for Continue {
    fn harness(&self) -> Harness {
        Harness::Continue
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.sessions.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };

        // Index entries from sessions.json, if present.
        let index = read_index(&sessions.join("sessions.json"));

        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Prefer the index, but only keep entries whose `<id>.json` actually exists.
        for meta in &index {
            let path = sessions.join(format!("{}.json", meta.session_id));
            if !path.exists() {
                continue;
            }
            if !seen.insert(meta.session_id.clone()) {
                continue;
            }
            out.push(ref_from(
                meta,
                &path,
                scan_session(&path).map_or(0, |s| s.message_count),
            ));
        }

        // Fold in any `<id>.json` on disk that the index didn't mention (tolerant of a missing or
        // stale index).
        if let Ok(rd) = fs::read_dir(sessions) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name == "sessions.json" || !name.ends_with(".json") {
                    continue;
                }
                let id = name.trim_end_matches(".json").to_string();
                if seen.contains(&id) {
                    continue;
                }
                let scan = scan_session(&p).unwrap_or_default();
                let meta = IndexMeta {
                    session_id: id,
                    title: scan.title,
                    cwd: scan.cwd,
                    created_at: None,
                };
                out.push(ref_from(&meta, &p, scan.message_count));
            }
        }

        Ok(out)
    }

    /// Project one accepted history item at a time, retaining body source ranges in lazy mode.
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Continue, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let mut s = Session {
                cwd: r.cwd.clone(),
                title: r.title.clone(),
                created_at: r.created_at,
                updated_at: r.updated_at.or_else(|| mtime(&r.path)),
                source_path: Some(r.path.clone()),
                ..Session::new(r.id.clone(), Harness::Continue)
            };
            sink.dependency(&r.path);
            if let Some(error) = sink.take_error() {
                return Err(error);
            }
            let Some(source) = JsonSource::capture(&r.path).ok() else {
                sink.meta(&s);
                return Ok(s);
            };
            let result = source.read_syntax(|root| {
                let Some(doc) = Fields::parse_struct(root, &["title", "workspaceDirectory", "history"])? else {
                    return Err(InvalidStruct("expected Continue session struct").into());
                };
                let title = record::title(doc.get("title"), true)?;
                let workspace = optional_string(doc.get("workspaceDirectory"))?;
                let history = doc.get("history").map(JsonView::accepted_array).transpose()?;
                if doc.get("history").is_some() && history.as_ref().is_some_and(Option::is_none) {
                    return Err(InvalidStruct("expected Continue history array").into());
                }
                if s.title.is_none() {
                    s.title = title;
                }
                if s.cwd.is_none() {
                    s.cwd = workspace.filter(|p| !p.is_empty()).map(|p| workspace_to_path(&p));
                }
                sink.meta(&s);
                let mut answered = std::collections::HashSet::<String>::new();
                let mut has_sources = false;
                for item in history.into_iter().flatten().flatten() {
                    let Some(item) = item? else { continue };
                    let reply_id = record::reply_id(item)?;
                    if reply_id.is_some_and(|id| answered.contains(&id)) {
                        continue;
                    }
                    let (message, results) = record::messages(item, opts.spans, &mut s.model)?;
                    if let Some(message) = message {
                        if s.system_prompt.is_none() && message.kind == MessageKind::SystemPrompt {
                            let mut text_message = message.clone();
                            text_message.try_materialize_text(&s.resolver())?;
                            let text = text_message.text();
                            if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                                s.system_prompt = Some(text);
                            }
                        }
                        let sourced = record::has_sources(&message);
                        has_sources |= sourced;
                        if sourced {
                            source.check()?;
                        }
                        let flow = sink.try_message(message)?;
                        if sourced {
                            source.check()?;
                        }
                        if flow == Flow::Stop {
                            break;
                        }
                    }
                    if let Some(results) = results {
                        for block in &results.content {
                            if let Block::ToolResult { tool_use_id, .. } = block {
                                answered.insert(tool_use_id.clone());
                            }
                        }
                        if sink.try_message(results)? == Flow::Stop {
                            break;
                        }
                    }
                }
                if has_sources {
                    source.check()?;
                }
                Ok(())
            });
            if let Err(error) = result {
                if !error.is::<InvalidJson>() && !error.is::<InvalidStruct>() {
                    return Err(error);
                }
                sink.meta(&s);
            }
            Ok(s)
        })
    }
}

fn optional_string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    let Some(view) = view else { return Ok(None) };
    if view.is_null()? {
        return Ok(None);
    }
    if !view.is_string()? || !view.accepts_value()? {
        return Err(InvalidStruct("expected optional string").into());
    }
    Ok(view.resolve()?.as_str().map(str::to_owned))
}

// ---------------------------------------------------------------------------
// Emit (IR -> Continue native layout)
// ---------------------------------------------------------------------------

/// Emit `session` into Continue's native layout under `out_dir` (the `sessions` directory):
///   `out_dir/<sessionId>.json`     — the transcript `{sessionId, title, workspaceDirectory, history}`
///   `out_dir/sessions.json`        — the index array (merged: our entry added/updated)
///
/// This is the faithful inverse of [`Continue::parse`] / [`Continue::discover`]:
/// - Each IR [`Message`] becomes one `history` item `{message, contextItems}`.
/// - `message` is the OpenAI-ish `ChatMessage` (`role`, string-or-array `content`, assistant
///   `toolCalls[]` with `arguments` as a JSON-encoded *string*, `role:"tool"` + `toolCallId`).
/// - `Block::File`/`Block::Image` on a turn become `contextItems[]` (so the parser's
///   `context_item_to_block` recovers them as `File` blocks); inline images also go in `content`.
///
/// The parser backfills `cwd`/`title` from the transcript itself, so writing the index is for
/// `discover()` fidelity, not correctness; we still write it.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<super::EmitResult> {
    super::wire::write::check_source_fields(session)?;
    use anyhow::Context;

    let new_id = opts.new_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    anyhow::ensure!(
        new_id != "sessions",
        "Continue session id 'sessions' is reserved for the native index"
    );
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    // Validate an existing index before publishing the transcript. Treating a corrupt or
    // unreadable index as empty would erase metadata for every other native conversation.
    let index_path = out_dir.join("sessions.json");
    let mut entries: Vec<Value> = match fs::read(&index_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("reading Continue index {}", index_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", index_path.display())),
    };

    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // ── transcript: <sessionId>.json ──
    let resolver = crate::lazy::Resolver::new(session.source_path.clone());
    let title = super::wire::write::fallback_title(session, Harness::Continue, opts.thinking, &resolver)?;
    let file_path = out_dir.join(format!("{new_id}.json"));
    emit::transcript(
        session,
        &new_id,
        title.as_deref(),
        cwd_str.as_deref(),
        &file_path,
        opts.thinking,
        opts.claim(),
    )?;

    // ── index: sessions.json (merge-or-create) ──
    // Drop any stale entry for this id, then append a fresh one.
    entries.retain(|e| e.get("sessionId").and_then(Value::as_str) != Some(new_id.as_str()));
    let created = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(chrono::Utc::now);
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        entry.insert("title".into(), json!(t));
    }
    // `dateCreated` is a string holding millisecond epoch (the older-build shape, which the parser
    // accepts alongside ISO).
    entry.insert("dateCreated".into(), json!(created.timestamp_millis().to_string()));
    if let Some(c) = &cwd_str {
        entry.insert("workspaceDirectory".into(), json!(c));
    }
    entries.push(Value::Object(entry));
    let mut stage = super::store::bytes::ByteStage::new();
    serde_json::to_writer_pretty(&mut stage, &entries)?;
    // Continue's own index: cv's update replaces it whole.
    super::store::publish_stage(stage, &index_path, crate::publish::Claim::Replace)?;

    Ok(super::EmitResult {
        path: file_path,
        new_id,
        resume_hint: None,
    })
}

mod emit;

#[cfg(test)]
/// Build one `history` item (`{message, contextItems}`) from an IR message.
fn history_item(msg: &Message, session_model: Option<&str>) -> Result<Value> {
    let role_str = match msg.role {
        Role::System => "system",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::User => "user",
    };

    let mut message = Map::new();
    message.insert("role".into(), json!(role_str));

    let mut context_items: Vec<Value> = Vec::new();

    match msg.role {
        Role::Tool => {
            // A tool result: Continue stores output in `content`, keyed by `toolCallId`.
            let (tool_use_id, content) = tool_result_of(&msg.content);
            message.insert("toolCallId".into(), json!(tool_use_id));
            message.insert("content".into(), json!(content));
        }
        _ => {
            // content: array of text/image parts (the array shape the parser handles).
            let mut parts: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => {
                        parts.push(json!({ "type": "text", "text": text }));
                    }
                    Block::Thinking { text, .. } => {
                        // Continue's ChatMessage has no thinking part; preserve as text so it's
                        // searchable and survives a round-trip as content.
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    Block::Image { data_ref, .. } => {
                        if let Some(url) = data_ref {
                            parts.push(json!({
                                "type": "imageUrl",
                                "imageUrl": { "url": url },
                            }));
                        }
                    }
                    Block::ToolUse { id, name, input, .. } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                // `arguments` is a JSON-encoded *string*.
                                "arguments": serde_json::to_string(input.resolve()?.as_ref())?,
                            },
                        }));
                    }
                    Block::File { path, source, mime } => {
                        context_items.push(file_context_item(path, source, mime));
                    }
                    Block::ToolResult { .. } => {}
                }
            }
            message.insert("content".into(), Value::Array(parts));
            if msg.role == Role::Assistant && !tool_calls.is_empty() {
                message.insert("toolCalls".into(), Value::Array(tool_calls));
            }
        }
    }

    // The owned control includes the same newly supported native usage field; body construction
    // remains independent of the checked writer and the native roundtrip pins numeric semantics.
    if msg.role == Role::Assistant {
        if let Some(usage) = &msg.usage {
            message.insert(
                "usage".into(),
                Value::Object(emit::native_usage(usage, msg.model.as_deref())?),
            );
        }
    }
    let mut item = Map::new();
    item.insert("message".into(), Value::Object(message));
    item.insert("contextItems".into(), Value::Array(context_items));
    // Carry the model so the parser can backfill it (assistant turns).
    if msg.role == Role::Assistant {
        if let Some(model) = msg.model.as_deref().or(session_model) {
            item.insert("promptLogs".into(), json!([{ "modelTitle": model }]));
        }
    }
    Ok(Value::Object(item))
}

/// Map a `Block::File` to a Continue `contextItem` (`{name, description, uri:{type, value}}`) that
/// the parser's `record::context_item` round-trips back into a `File` block.
fn file_context_item(path: &Option<String>, source: &Option<String>, mime: &Option<String>) -> Value {
    let mut ci = Map::new();
    match (path, source) {
        (Some(p), _) => {
            ci.insert("name".into(), json!(file_name_of(p)));
            ci.insert("description".into(), json!(p));
            ci.insert("uri".into(), json!({ "type": "file", "value": p }));
        }
        (None, Some(s)) => {
            ci.insert("name".into(), json!(s));
            ci.insert("description".into(), json!(s));
            ci.insert("uri".into(), json!({ "type": "url", "value": s }));
        }
        (None, None) => {
            ci.insert("name".into(), json!("file"));
        }
    }
    if let Some(m) = mime {
        ci.insert("content".into(), json!(""));
        ci.insert("mime".into(), json!(m));
    }
    Value::Object(ci)
}

/// Basename of a path-ish string, for a `contextItem` name.
fn file_name_of(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

/// Pull `(toolCallId, content)` from a Tool turn's first ToolResult block.
#[cfg(test)]
fn tool_result_of(content: &[Block]) -> (String, String) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id, content, ..
        } = b
        {
            return (tool_use_id.clone(), content.to_string());
        }
    }
    (String::new(), String::new())
}

// ---------------------------------------------------------------------------
// Index (sessions.json)
// ---------------------------------------------------------------------------

struct IndexMeta {
    session_id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
}

/// Read `sessions.json` (array of `{sessionId, title, dateCreated, workspaceDirectory}`). Empty vec
/// on any error.
fn read_index(path: &Path) -> Vec<IndexMeta> {
    let Some(source) = JsonSource::capture(path).ok() else {
        return Vec::new();
    };
    source
        .read_document(|root| {
            let mut out = Vec::new();
            if let Some(entries) = root.array()? {
                for entry in entries {
                    let Some(fields) =
                        Fields::parse(entry?, &["sessionId", "title", "dateCreated", "workspaceDirectory"])?
                    else {
                        continue;
                    };
                    let Some(session_id) = fields.string("sessionId")?.filter(|s| !s.is_empty()) else {
                        continue;
                    };
                    out.push(IndexMeta {
                        session_id,
                        title: record::title(fields.get("title"), false)?,
                        cwd: fields
                            .string("workspaceDirectory")?
                            .filter(|p| !p.is_empty())
                            .map(|p| workspace_to_path(&p)),
                        created_at: fields.value("dateCreated")?.as_ref().and_then(parse_date),
                    });
                }
            }
            Ok(out)
        })
        .unwrap_or_default()
}

/// Build a `SessionRef` from index metadata + the on-disk transcript path.
fn ref_from(meta: &IndexMeta, path: &Path, message_count: usize) -> SessionRef {
    let updated = mtime(path);
    SessionRef {
        id: meta.session_id.clone(),
        harness: Harness::Continue,
        path: path.to_path_buf(),
        cwd: meta.cwd.clone(),
        title: meta.title.clone(),
        created_at: meta.created_at.or(updated),
        updated_at: updated,
        message_count,
    }
}

/// Discovery keeps metadata and a count, never a whole history. Validate each discarded item
/// before dropping it: IgnoredAny would accept malformed Unicode that the old Value rejected.
/// Duplicate fields retain their last value, including null/wrong-type replacements.
#[derive(Default)]
struct Scan {
    title: Option<String>,
    cwd: Option<PathBuf>,
    message_count: usize,
}

fn scan_session(path: &Path) -> Option<Scan> {
    let source = JsonSource::capture(path).ok()?;
    source
        .read_document(|root| {
            let mut scan = Scan::default();
            if let Some(fields) = Fields::parse(root, &["title", "workspaceDirectory", "history"])? {
                scan.title = record::title(fields.get("title"), false)?;
                scan.cwd = fields
                    .string("workspaceDirectory")?
                    .filter(|p| !p.is_empty())
                    .map(|p| workspace_to_path(&p));
                if let Some(history) = fields.get("history").map(JsonView::array).transpose()?.flatten() {
                    for item in history {
                        item?;
                        scan.message_count += 1;
                    }
                }
            }
            Ok(scan)
        })
        .ok()
}

// ---------------------------------------------------------------------------
// history[] -> IR
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Continue's `workspaceDirectory` may be a `file://` URI or a plain path. Normalize to a path.
fn workspace_to_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("file://") {
        // file:///Users/... -> /Users/... (drop the empty authority's leading slash group).
        PathBuf::from(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Parse a `dateCreated` value: a number or numeric-string of **milliseconds** since epoch, or an
/// ISO-8601 string.
fn parse_date(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => n.as_i64().and_then(ts_from_epoch),
        Value::String(s) => {
            let s = s.trim();
            if let Ok(ms) = s.parse::<i64>() {
                return ts_from_epoch(ms);
            }
            super::parse_ts(s)
        }
        _ => None,
    }
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_preserves_existing_files_when_index_is_unusable() {
        let world = crate::test_world("continue-index-export-error");
        let dir = world.base.join("sessions");
        fs::create_dir(&dir).unwrap();
        let transcript = dir.join("fixed.json");
        fs::write(&transcript, "original transcript").unwrap();
        let index = dir.join("sessions.json");
        let session = Session::new("source", Harness::Continue);
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        for unusable in ["{broken", "{}", "null"] {
            fs::write(&index, unusable).unwrap();
            assert!(emit(&session, &dir, &opts).is_err());
            assert_eq!(fs::read_to_string(&index).unwrap(), unusable);
            assert_eq!(fs::read_to_string(&transcript).unwrap(), "original transcript");
        }
        let reserved = crate::emit::EmitOptions {
            new_id: Some("sessions".into()),
            ..Default::default()
        };
        fs::write(&index, "[]").unwrap();
        assert!(emit(&session, &dir, &reserved).is_err());
        assert_eq!(fs::read_to_string(&index).unwrap(), "[]");
        fs::write(&index, r#"[{"sessionId":"other","custom":{"retain":true}}]"#).unwrap();
        // `fixed.json` is taken: refused without force, replaced (and indexed) with it.
        assert!(emit(&session, &dir, &opts)
            .unwrap_err()
            .downcast_ref::<crate::publish::Exists>()
            .is_some());
        assert_eq!(fs::read_to_string(&transcript).unwrap(), "original transcript");
        let forced = crate::emit::EmitOptions { force: true, ..opts };
        emit(&session, &dir, &forced).unwrap();
        let merged: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        assert_eq!(merged[0]["custom"]["retain"], true);
        assert_eq!(merged[1]["sessionId"], "fixed");
    }
    use crate::harness::Parse;

    /// The history-item codec over an owned value, no spans.
    fn history_item_to_message(item: &Value, model: &mut Option<String>) -> Option<Message> {
        record::messages(item.into(), false, model).unwrap().0
    }

    /// Source-backed items with spans resolve to exactly what the owned value gives.
    #[test]
    fn history_items_resolve_alike_from_source_and_value_and_header_acceptance() {
        #[derive(serde::Deserialize)]
        struct OldDoc<'a> {
            #[serde(default)]
            title: Option<String>,
            #[serde(default, rename = "workspaceDirectory")]
            workspace: Option<String>,
            #[serde(default, borrow)]
            history: Vec<&'a serde_json::value::RawValue>,
        }
        let world = cv_testkit::World::new("continue-view-oracle");
        let path = world.base.join("s.json");
        let body = "雪\n\"\\".repeat(8192);
        let items = json!([
            {"message":{"role":"user","content":[{"text":""},{"type":"ignored","text":body},{"type":"imageUrl","imageUrl":{"url":"file:///image"}}]},
                "contextItems":[{"name":"a","description":"file","content":body,"uri":{"type":"file","value":"a.rs"}}]},
            {"message":{"role":"assistant","content":body,
                "toolCalls":[{"id":"a","function":{"name":"read","arguments":{"payload":body}}},{"id":"b","function":{"name":"edit","arguments":serde_json::to_string(&json!({"payload":body})).unwrap()}}],
                "usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40,"ignored":body},"model":"m","cost_cents":2,"ignored":body}},
                "promptLogs":[{"modelTitle":null,"modelProvider":"suppressed","ignored":body}],
                "toolCallStates":[{"toolCallId":"a","status":"errored","toolCall":{"function":{"name":"read"}},"output":[{"content":body,"ignored":body}]}]},
            {"message":{"role":"tool","toolCallId":"a","content":[{"text":""},{"text":body}]}},
            {"message":null,"contextItems":[{"name":"still a file","content":body}]},
            {"message":{"role":"system","content":body}}
        ]);
        fs::write(&path, serde_json::to_vec(&items).unwrap()).unwrap();
        let source = JsonSource::capture(&path).unwrap();
        source
            .read_document(|root| {
                let mut expected_model = None;
                let mut actual_model = None;
                for (view, owned) in root.array()?.unwrap().zip(items.as_array().unwrap()) {
                    let expected = record::messages(owned.into(), false, &mut expected_model)?;
                    let mut actual = record::messages(view?, true, &mut actual_model)?;
                    if let Some(message) = &actual.0 {
                        if owned.pointer("/message/toolCalls").is_some() {
                            assert!(record::has_sources(message));
                        }
                    }
                    let resolver = crate::lazy::Resolver::new(Some(path.clone()));
                    for message in [&mut actual.0, &mut actual.1].into_iter().flatten() {
                        message.materialize(&resolver)?;
                    }
                    assert_eq!(
                        serde_json::to_string(&actual).unwrap(),
                        serde_json::to_string(&expected).unwrap()
                    );
                    assert_eq!(actual_model, expected_model);
                }
                Ok(())
            })
            .unwrap();
        for text in [
            "{}",
            "[]",
            "[null,null,[]]",
            "[\"title\",\"/cwd\",[]]",
            "[null,null,[],0]",
            "{\"title\":null,\"history\":[]}",
            "{\"title\":1,\"history\":[]}",
            "{\"title\":\"a\",\"title\":\"b\",\"history\":[]}",
            "{\"history\":null}",
            "{\"ignored\":1e400,\"history\":[{\"message\":{\"content\":\"yes\"}}]}",
            "{\"title\":\"\\ud800\",\"history\":[]}",
            "{\"ignored\":\"\\ud800\",\"history\":[]}",
        ] {
            fs::write(&path, text).unwrap();
            let reference = SessionRef::new("s", Harness::Continue, path.clone());
            let mut sink = crate::stream::CollectSink::default();
            let actual = Continue { sessions: None }
                .stream(&reference, &ParseOptions::lazy(), &mut sink)
                .unwrap();
            match serde_json::from_str::<OldDoc<'_>>(text) {
                Ok(old) => {
                    let mut model = None;
                    let expected: Vec<_> = old
                        .history
                        .iter()
                        .filter_map(|v| serde_json::from_str::<Value>(v.get()).ok())
                        .filter_map(|v| history_item_to_message(&v, &mut model))
                        .collect();
                    assert_eq!(sink.messages.len(), expected.len(), "{text}");
                    assert_eq!(
                        actual.title,
                        old.title
                            .filter(|t| !t.trim().is_empty())
                            .map(|t| crate::ir::truncate(&t, 80)),
                        "{text}"
                    );
                    assert_eq!(
                        actual.cwd,
                        old.workspace.filter(|w| !w.is_empty()).map(|w| workspace_to_path(&w)),
                        "{text}"
                    );
                }
                Err(_) => {
                    assert!(sink.messages.is_empty(), "{text}");
                    assert!(actual.title.is_none(), "{text}");
                }
            }
        }
    }

    #[test]
    fn encoded_arguments_survive_minimum_message_deferred_staging() {
        let world = crate::test_world("continue-encoded-minimum-staging");
        let path = world.base.join("session.json");
        let argument = serde_json::to_string(&json!({"payload":"x".repeat(8192)})).unwrap();
        let history = (0..3).map(|i| json!({"message":{"role":"assistant","usage":{"model":(i == 2).then_some("present")},"toolCalls":[{"id":i.to_string(),"function":{"name":"read","arguments":argument}}]}})).collect::<Vec<_>>();
        fs::write(&path, serde_json::to_vec(&json!({"history":history})).unwrap()).unwrap();
        let reference = SessionRef::new("fixture", Harness::Continue, &path);
        let adapter = Continue { sessions: None };
        let full = adapter.parse(&reference).unwrap();
        let mut expected = Vec::new();
        crate::dataset::write_record(&full, &mut expected, crate::dataset::Format::Chatml, None).unwrap();
        expected.push(b'\n');
        for minimum in [1, 2, 3] {
            let staged = crate::dataset::stage_record(
                &adapter,
                &reference,
                crate::dataset::Format::Chatml,
                None,
                minimum,
                None,
                &Default::default(),
            )
            .unwrap()
            .unwrap();
            let mut actual = Vec::new();
            staged.publish(&mut actual).unwrap();
            assert!(actual == expected);
        }
        assert!(crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            4,
            None,
            &Default::default()
        )
        .unwrap()
        .is_none());
        for (query, matched) in [("model:present", true), ("model:absent", false)] {
            let query = crate::SessionQuery::parse(query).unwrap();
            let staged = crate::dataset::stage_record(
                &adapter,
                &reference,
                crate::dataset::Format::Chatml,
                None,
                0,
                Some(&query),
                &Default::default(),
            )
            .unwrap();
            assert_eq!(staged.is_some(), matched);
            if let Some(staged) = staged {
                let mut actual = Vec::new();
                staged.publish(&mut actual).unwrap();
                assert!(actual == expected);
            }
        }
    }

    #[test]
    fn published_arguments_reject_source_changes_after_a_stopping_callback() {
        let world = cv_testkit::World::new("continue-source-change");
        let path = world.base.join("s.json");
        fs::write(&path, serde_json::to_vec(&json!({"history":[{"message":{"role":"assistant","toolCalls":[{"id":"a","function":{"name":"read","arguments":{"payload":"x".repeat(8192)}}}]}}]})).unwrap()).unwrap();
        let reference = SessionRef::new("s", Harness::Continue, path.clone());
        let mut published = None;
        let mut mutate = |message: Message| {
            if let Some(input) = message.content.iter().find_map(|block| match block {
                Block::ToolUse {
                    input: crate::Json::Source(_),
                    ..
                } => {
                    if let Block::ToolUse { input, .. } = block {
                        Some(input)
                    } else {
                        None
                    }
                }
                _ => None,
            }) {
                published = Some(input.clone());
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b" ")
                    .unwrap();
                Flow::Stop
            } else {
                Flow::Continue
            }
        };
        assert!(Continue { sessions: None }
            .stream(&reference, &ParseOptions::lazy(), &mut mutate)
            .is_err());
        assert!(published.unwrap().resolve().is_err());
    }

    #[test]
    fn cn_tool_call_states_become_tool_results_and_usage_is_read() {
        let dir = std::env::temp_dir().join(format!("cv-continue-cn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        fs::write(&path, r#"{"sessionId":"s","history":[
            {"message":{"role":"user","content":"read a"},"contextItems":[]},
            {"message":{"role":"assistant","content":"","toolCalls":[{"id":"c1","type":"function","function":{"name":"Read","arguments":"{}"}}],
              "usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40},"model":"glm","cost_cents":2}},
             "contextItems":[],
             "toolCallStates":[{"toolCallId":"c1","toolCall":{"function":{"name":"Read"}},"status":"errored",
               "output":[{"content":"no such file","name":"Tool Result"}]}]},
            {"message":{"role":"tool","content":"no such file","toolCallId":"c1"},"contextItems":[]}
        ]}"#).unwrap();
        let r = SessionRef::new("s", Harness::Continue, path);
        let s = Continue { sessions: None }.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [MessageKind::Prompt, MessageKind::Reply, MessageKind::ToolResult]
        );
        let u = s.messages[1].usage.as_ref().unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (Some(60), Some(40), Some(7))
        );
        assert_eq!(s.model.as_deref(), Some("glm"));
        match &s.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                is_error,
                tool_name,
                ..
            } => {
                assert_eq!(
                    (tool_use_id.as_str(), *is_error, tool_name.as_deref()),
                    ("c1", true, Some("Read"))
                )
            }
            b => panic!("{b:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/continue")
    }

    #[test]
    fn date_parsing_ms_and_iso() {
        let ms = parse_date(&Value::String("1700000000000".into())).unwrap();
        assert_eq!(ms.timestamp_millis(), 1700000000000);
        let num = parse_date(&Value::Number(1700000000000i64.into())).unwrap();
        assert_eq!(num.timestamp_millis(), 1700000000000);
        let iso = parse_date(&Value::String("2026-05-29T12:00:00Z".into())).unwrap();
        assert_eq!(iso.to_rfc3339(), "2026-05-29T12:00:00+00:00");
        assert!(parse_date(&Value::Null).is_none());
    }

    #[test]
    fn workspace_uri_and_plain() {
        assert_eq!(workspace_to_path("/Users/me/proj"), PathBuf::from("/Users/me/proj"));
        assert_eq!(
            workspace_to_path("file:///Users/me/proj"),
            PathBuf::from("/Users/me/proj")
        );
    }

    #[test]
    fn user_string_and_assistant_toolcall() {
        let mut model = None;
        let user: Value =
            serde_json::from_str(r#"{"message":{"role":"user","content":"hello"},"contextItems":[]}"#).unwrap();
        let m = history_item_to_message(&user, &mut model).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let asst: Value = serde_json::from_str(
            r#"{"message":{"role":"assistant","content":"let me check","toolCalls":[
                {"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}
            ]}}"#,
        )
        .unwrap();
        let m = history_item_to_message(&asst, &mut model).unwrap();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.text().as_deref(), Some("let me check"));
        match m.content.iter().find(|b| matches!(b, Block::ToolUse { .. })) {
            Some(Block::ToolUse { id, name, input, .. }) => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(input.resolve().unwrap()["path"], "a.rs");
            }
            _ => panic!("expected tool_use"),
        }
    }

    #[test]
    fn content_array_text_and_image() {
        let mut model = None;
        let v: Value = serde_json::from_str(
            r#"{"message":{"role":"user","content":[
                {"type":"text","text":"look at this"},
                {"type":"imageUrl","imageUrl":{"url":"data:image/png;base64,AAA"}}
            ]}}"#,
        )
        .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        assert_eq!(m.text().as_deref(), Some("look at this"));
        assert!(m.content.iter().any(|b| matches!(
            b,
            Block::Image { data_ref, .. } if data_ref.as_deref() == Some("data:image/png;base64,AAA")
        )));
    }

    #[test]
    fn kind_and_origin_across_roles() {
        let mut model = None;
        let mk = |line: &str, model: &mut Option<String>| {
            history_item_to_message(&serde_json::from_str::<Value>(line).unwrap(), model).unwrap()
        };
        let u = mk(r#"{"message":{"role":"user","content":"hi"}}"#, &mut model);
        assert_eq!((u.kind, u.origin), (MessageKind::Prompt, Origin::Human));
        let a = mk(r#"{"message":{"role":"assistant","content":"yo"}}"#, &mut model);
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        let t = mk(
            r#"{"message":{"role":"tool","content":"out","toolCallId":"c1"}}"#,
            &mut model,
        );
        assert_eq!((t.kind, t.origin), (MessageKind::ToolResult, Origin::Harness));
        let sys = mk(r#"{"message":{"role":"system","content":"you are terse"}}"#, &mut model);
        assert_eq!(
            (sys.role, sys.kind, sys.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );
        // No flat extra keys were introduced.
        for m in [&u, &a, &t, &sys] {
            for k in m.extra.keys() {
                assert!(k == "continue" || k == "_record", "unexpected extra key {k}");
            }
        }
    }

    #[test]
    fn tool_result_message() {
        let mut model = None;
        let v: Value =
            serde_json::from_str(r#"{"message":{"role":"tool","content":"file contents here","toolCallId":"call_1"}}"#)
                .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id, content, ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file contents here");
            }
            _ => panic!("expected tool_result"),
        }
    }

    #[test]
    fn context_items_become_files() {
        let mut model = None;
        let v: Value = serde_json::from_str(
            r#"{"message":{"role":"user","content":"check these"},"contextItems":[
                {"name":"main.rs","description":"src/main.rs","content":"fn main(){}","uri":{"type":"file","value":"/proj/src/main.rs"}},
                {"name":"Docs","description":"react docs","content":"...","uri":{"type":"url","value":"https://react.dev"}}
            ]}"#,
        )
        .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        let files: Vec<_> = m
            .content
            .iter()
            .filter_map(|b| match b {
                Block::File { path, source, .. } => Some((path.clone(), source.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0.as_deref(), Some("/proj/src/main.rs"));
        assert_eq!(files[1].1.as_deref(), Some("https://react.dev"));
    }

    #[test]
    fn parses_fixture_session() {
        let dir = fixtures();
        let path = dir.join("11111111-1111-1111-1111-111111111111.json");
        if !path.exists() {
            return;
        }
        let r = SessionRef::new("11111111-1111-1111-1111-111111111111", Harness::Continue, path.clone());
        let s = Continue { sessions: None }.parse(&r).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::File { .. }))));
    }

    #[test]
    fn emit_round_trips() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-continue-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);

        // Build a small session: user, assistant w/ a tool call, a tool result.
        let mut session = Session {
            cwd: Some(PathBuf::from("/Users/me/proj")),
            title: Some("Roundtrip".into()),
            model: Some("gpt-4o".into()),
            ..Session::new("src", Harness::Continue)
        };

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "please read a.rs".into(),
        });
        session.messages.push(user);

        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Text { text: "on it".into() });
        asst.content.push(Block::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({ "path": "a.rs" }).into(),
            namespace: None,
        });
        session.messages.push(asst);

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        session.messages.push(tool);

        let opts = crate::emit::EmitOptions {
            new_id: Some("emit-test-id".into()),
            new_cwd: None,
            ..Default::default()
        };
        let res = emit(&session, &dir, &opts).unwrap();
        assert_eq!(res.new_id, "emit-test-id");
        assert!(res.path.exists());

        // Re-parse via this adapter.
        let r = SessionRef::new(res.new_id.clone(), Harness::Continue, res.path.clone());
        let parsed = Continue { sessions: None }.parse(&r).unwrap();

        assert_eq!(parsed.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[0].text().as_deref(), Some("please read a.rs"));
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert!(parsed.messages[1]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "read_file")));
        assert_eq!(parsed.messages[2].role, Role::Tool);
        assert!(parsed.messages[2].content.iter().any(|b| matches!(
            b,
            Block::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_1" && content == "fn main() {}"
        )));

        // The index should also discover it.
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let refs = adapter.discover().unwrap();
        assert!(refs.iter().any(|r| r.id == "emit-test-id"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovers_from_index() {
        let dir = fixtures();
        if !dir.join("sessions.json").exists() {
            return;
        }
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let refs = adapter.discover().unwrap();
        let r = refs
            .iter()
            .find(|r| r.id == "11111111-1111-1111-1111-111111111111")
            .expect("indexed session discovered");
        assert_eq!(r.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert!(r.message_count >= 3);
        assert!(r.created_at.is_some());
    }

    /// The native `stream` must emit incrementally (not collect-then-replay): a sink that stops on
    /// the first message ends the parse with exactly one message seen, never materializing the rest.
    #[test]
    fn stream_is_incremental_and_honors_stop() {
        use crate::stream::{Flow, MessageSink};
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-continue-stream-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        fs::write(
            &path,
            r#"{"sessionId":"s","title":"T","workspaceDirectory":"/w","history":[
                {"message":{"role":"user","content":"one"}},
                {"message":{"role":"assistant","content":"two"}},
                {"message":{"role":"user","content":"three"}}
            ]}"#,
        )
        .unwrap();

        let r = SessionRef::new("s", Harness::Continue, path.clone());

        // A sink that records every message it sees and stops after the first.
        struct StopFirst {
            seen: Vec<String>,
        }
        impl MessageSink for StopFirst {
            fn message(&mut self, m: Message) -> Flow {
                self.seen.push(m.text().unwrap_or_default());
                Flow::Stop
            }
        }
        let mut sink = StopFirst { seen: Vec::new() };
        let s = Continue { sessions: None }
            .stream(&r, &ParseOptions::full(), &mut sink)
            .unwrap();

        // Only the first message was ever materialized; the returned Session is metadata-only.
        assert_eq!(sink.seen, vec!["one".to_string()]);
        assert!(s.messages.is_empty(), "stream returns empty messages");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/w")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_scan_matches_whole_value_metadata() {
        let _world = crate::test_world("continue-scan");
        let dir = std::env::temp_dir().join(format!("cv-continue-scan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let mut cases = vec![
            r#"{"title":"escaped\n\uD834\uDD1E","workspaceDirectory":"file:///p","history":[null,{},42,{"message":{"role":"tool"}}]}"#.to_owned(),
            r#"{"title":"old","title":null,"workspaceDirectory":"/old","workspaceDirectory":42,"history":[1,2],"history":false}"#.to_owned(),
            r#"{"title":42,"title":"new","history":null,"history":[1,2,3]}"#.to_owned(),
            r#"{"history":{"nested":[1,2]},"title":"   "}"#.to_owned(),
            "[1,2,3]".to_owned(), "null".to_owned(), "42".to_owned(),
            r#"{"title":"early","history":[1,{"content":"bad\uD800"}]}"#.to_owned(),
            r#"{"history":[1],"ignored":"bad\uDC00"}"#.to_owned(),
            r#"{"history":[1e999]}"#.to_owned(),
            r#"{"history":[1],"history":[2,}"#.to_owned(),
            r#"{"history":[1]} trailing"#.to_owned(),
        ];
        for depth in 123..131 {
            cases.push(format!(
                r#"{{"history":[{}0{}]}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        // Large discarded bodies and nested context are validated, not retained in a history tree.
        cases.push(
            serde_json::json!({
                "title": "metadata", "history": (0..24).map(|_| serde_json::json!({
                    "message": { "content": "payload\\\n".repeat(16_384) },
                    "contextItems": [{"content": "unused".repeat(4096)}],
                })).collect::<Vec<_>>()
            })
            .to_string(),
        );
        for text in cases {
            fs::write(&path, &text).unwrap();
            let expected = serde_json::from_str::<Value>(&text).ok().map(|v| {
                let title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| crate::ir::truncate(t, 80));
                let cwd = v
                    .get("workspaceDirectory")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(workspace_to_path);
                let count = v.get("history").and_then(Value::as_array).map_or(0, Vec::len);
                (title, cwd, count)
            });
            let actual = scan_session(&path).map(|s| (s.title, s.cwd, s.message_count));
            assert_eq!(actual, expected, "case prefix: {:?}", &text[..text.len().min(120)]);
        }
        fs::write(&path, b"{\"history\":[\"\xff\"]}").unwrap();
        assert!(scan_session(&path).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovery_scan_preserves_index_precedence_and_unindexed_fallback() {
        let _world = crate::test_world("continue-scan-index");
        let dir = std::env::temp_dir().join(format!("cv-continue-index-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("sessions.json"),
            r#"[
            {"sessionId":"indexed","title":"index title","workspaceDirectory":"/index","dateCreated":"1700000000000"},
            {"sessionId":"indexed","title":"duplicate entry"},
            {"sessionId":"missing","title":"missing"}
        ]"#,
        )
        .unwrap();
        fs::write(
            dir.join("indexed.json"),
            r#"{"title":"file title","workspaceDirectory":"/file","history":[1,2]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("extra.json"),
            r#"{"title":"extra title","workspaceDirectory":"file:///extra","history":[1,null,{}]}"#,
        )
        .unwrap();
        fs::write(dir.join("broken.json"), r#"{"title":"partial","history":[1,}"#).unwrap();
        let refs = Continue {
            sessions: Some(dir.clone()),
        }
        .discover()
        .unwrap();
        assert_eq!(refs.len(), 3);
        let indexed = refs.iter().find(|r| r.id == "indexed").unwrap();
        assert_eq!(indexed.title.as_deref(), Some("index title"));
        assert_eq!(indexed.cwd.as_deref(), Some(Path::new("/index")));
        assert_eq!(indexed.created_at.unwrap().timestamp_millis(), 1700000000000);
        assert_eq!(indexed.message_count, 2);
        let extra = refs.iter().find(|r| r.id == "extra").unwrap();
        assert_eq!(extra.title.as_deref(), Some("extra title"));
        assert_eq!(extra.cwd.as_deref(), Some(Path::new("/extra")));
        assert_eq!(extra.created_at, extra.updated_at);
        assert_eq!(extra.message_count, 3);
        let broken = refs.iter().find(|r| r.id == "broken").unwrap();
        assert!(broken.title.is_none());
        assert_eq!(broken.message_count, 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
