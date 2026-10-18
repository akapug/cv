//! OpenCode adapter — `${XDG_DATA_HOME:-~/.local/share}/opencode/`.
//!
//! ## Where sessions live
//!
//! Since 2026-01 (opencode ≥ 1.1) the canonical store is a SQLite database, `opencode.db`
//! (`$OPENCODE_DB` overrides it; non-release channels write `opencode-<channel>.db`; WAL mode):
//! `session` rows (one per session — `directory` = cwd, `title`, `model`, `agent`, cost and token
//! totals), `message` rows (`data` = the message Info JSON minus `id`/`sessionID`) and `part` rows
//! (`data` = the Part JSON minus `id`/`sessionID`/`messageID`), keyed by `message_id`. Source of
//! truth: `packages/core/src/session/sql.ts` (tables) and `packages/schema/src/v1/session.ts`
//! (payload shapes) at opencode `fee476bb` (1.18). The older JSON tree under `storage/` —
//! `session/**/ses_*.json`, `message/<sid>/<msgid>.json`, `part/<msgid>/<partid>.json` (parts keyed
//! by *messageID*) — was only ever the input to a one-shot importer that was deleted on 2026-06-02:
//! a machine that ran the import still carries a frozen copy, a fresh install has none. We read the
//! database when it exists (behind the `sqlite` cargo feature) and fall back to the JSON tree,
//! de-duplicating by session id (db wins). The payload JSON is identical in both, so one codec
//! (`record.rs`, over a file's or a cell's view) serves both; db-backed [`SessionRef`]s point at the `.db` file.
//!
//! ## Two storage generations (JSON tree)
//!
//! OpenCode rewrote its on-disk model. We sniff by content (presence of separate part files /
//! an inline `summary` object) rather than any version field:
//!
//! * **Inline-summary generation (older):** a message carried an AI-generated `summary`
//!   *object* `{title, body, diffs[]}` inline; there were no separate part files. We render
//!   `summary.body` (falling back to `title`) as the message text and stash `diffs` in `extra`.
//! * **Parts generation (newer):** message content lives in `part/<msgid>/<partid>.json`
//!   files, one per part. The message record holds only metadata. The richest variant.
//!   Confusingly, a *boolean* `summary: true` here is a compaction marker on an assistant
//!   message (mode/agent == "compaction"), NOT inline content — we keep it in `extra`.
//!
//! The on-disk part taxonomy (source of truth: opencode `session/message-v2.ts`) is:
//! `text` (may be `synthetic`/`ignored`), `reasoning` (carries `metadata.anthropic.signature`),
//! `tool` (with a `state` union pending|running|completed|error, each holding
//! input/output/title/metadata/error/time), `file` (file/dir/image attachment with `mime`,
//! `url`, `source`), `agent` (an `@agent` mention / sub-agent reference), `subtask` (a spawned
//! sub-session with its own prompt/agent/model), `patch`, `snapshot`, `step-start`,
//! `step-finish` (per-step cost/tokens), `retry`, `compaction`.
//!
//! ## Kinds (`docs/INTERFACE-V2.md` §4)
//!
//! A `user` record is a `Prompt`/Human and an `assistant` record a `Reply`/Model, except: a record
//! whose parts include `compaction` (the user-side request that starts a compaction — on this
//! machine always a user record with that one part) is the `CompactionBoundary`; an assistant
//! record with `summary: true` (`agent: compaction`) is the `CompactionSummary`; an assistant record
//! carrying `error` is an `Error` (`extra["opencode"]["error"]` + `error_name`), whatever partial
//! text it kept; a record with an `agent` mention or a `subtask` part is a `SubagentSpawn`; a
//! `system` record is the `SystemPrompt` (also `Session::system_prompt`). Compaction/error origin
//! is Harness. Tool results are split into a trailing `ToolResult`/Harness message.
//! `step-start`/`step-finish`/`retry`/`snapshot`/`patch` parts never surface as messages; they ride
//! on their record's `extra["opencode"]` bag (`steps`, `retries`, `snapshots`, `patches`), as do
//! `agent_refs`, `subtasks`, `compactions`, `unknown_parts`, the record's `agent`/`mode`/
//! `providerID`/`finish`/`variant`/`structured`/`format`/`system`/`tools`/`cwd`, `summary_diffs`
//! (old generation) and `error`/`error_name`. `cost` and `tokens.reasoning` are first-class
//! (`Usage::cost_usd`, `Usage::reasoning_tokens`); `session.parent_id` is `Lineage::parent`; the
//! remaining `session` columns land in `Session::extra["opencode"]`.

mod emit;
mod record;
#[cfg(any(feature = "sqlite", test))]
mod v2;
pub use emit::emit;

use super::Adapter;
use crate::ir::*;
use crate::source::{is_sqlite_source, Dependencies, Probe};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct OpenCode {
    /// The legacy JSON tree (`<data>/opencode/storage`), if present.
    root: Option<PathBuf>,
    /// The canonical SQLite store, if present (and the `sqlite` feature is on).
    db: Option<PathBuf>,
}

impl OpenCode {
    pub fn detect() -> Self {
        let data = data_dir();
        let root = data.as_ref().map(|d| d.join("storage")).filter(|p| p.exists());
        let db = data.as_deref().and_then(resolve_db);
        OpenCode { root, db }
    }

    /// An adapter rooted at an explicit JSON storage dir (`…/opencode/storage`) instead of `$HOME` —
    /// lets emit-verification and tests re-parse a just-written session without mutating the
    /// process environment (env writes race other threads' `getenv`). No database.
    pub fn with_root(root: PathBuf) -> Self {
        OpenCode {
            root: Some(root),
            db: None,
        }
    }

    /// An adapter over an explicit OpenCode data dir (`…/opencode`, holding `opencode.db` and maybe
    /// the old `storage/` tree), read the way [`detect`](Self::detect) reads the real one but without
    /// consulting `$OPENCODE_DB` (the corpus test, other machines' copies).
    pub fn with_data_dir(data: &Path) -> Self {
        let root = Some(data.join("storage")).filter(|p| p.exists());
        #[cfg(feature = "sqlite")]
        let db = Some(data.join("opencode.db")).filter(|p| p.is_file());
        #[cfg(not(feature = "sqlite"))]
        let db = None;
        OpenCode { root, db }
    }

    /// An adapter over one explicit `opencode.db` (tests, alternate data dirs). No JSON tree.
    #[cfg(feature = "sqlite")]
    pub fn with_db(db: PathBuf) -> Self {
        OpenCode {
            root: None,
            db: Some(db),
        }
    }

    /// Sessions from the SQLite store, plus their ids so the JSON walk can skip duplicates.
    #[cfg(feature = "sqlite")]
    fn discover_db(&self) -> (Vec<SessionRef>, HashSet<String>) {
        let Some(db) = &self.db else {
            return (Vec::new(), HashSet::new());
        };
        match db::discover(db) {
            Ok(refs) => {
                let ids = refs.iter().map(|r| r.id.clone()).collect();
                (refs, ids)
            }
            Err(e) => {
                eprintln!("cv: skipping {}: {e:#}", db.display());
                (Vec::new(), HashSet::new())
            }
        }
    }
    #[cfg(not(feature = "sqlite"))]
    fn discover_db(&self) -> (Vec<SessionRef>, HashSet<String>) {
        (Vec::new(), HashSet::new())
    }

    fn message_dir(&self, sid: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("message").join(sid))
    }
    fn part_dir(&self, mid: &str) -> Option<PathBuf> {
        // Parts are keyed by messageID only: storage/part/<messageID>/<partid>.json
        self.root.as_ref().map(|r| r.join("part").join(mid))
    }
}

/// `${XDG_DATA_HOME:-~/.local/share}/opencode` — opencode's data dir (`packages/core/src/global.ts`,
/// via `xdg-basedir`).
fn data_dir() -> Option<PathBuf> {
    Some(super::store::xdg_data_dir()?.join("opencode"))
}

/// The database opencode itself would open (`packages/core/src/database/database.ts::path`):
/// `$OPENCODE_DB` (absolute, or relative to the data dir; `:memory:` means none), else
/// `opencode.db` (release channels), else the newest `opencode-<channel>.db`.
#[cfg(feature = "sqlite")]
fn resolve_db(data: &Path) -> Option<PathBuf> {
    if let Some(v) = crate::env::var_os("OPENCODE_DB") {
        if v == ":memory:" {
            return None;
        }
        let v = PathBuf::from(v);
        let p = if v.is_absolute() { v } else { data.join(v) };
        return p.is_file().then_some(p);
    }
    let main = data.join("opencode.db");
    if main.is_file() {
        return Some(main);
    }
    let mut channel: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(data)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if !(name.starts_with("opencode-") && name.ends_with(".db")) {
                return None;
            }
            Some((e.metadata().ok()?.modified().ok()?, p))
        })
        .collect();
    channel.sort();
    channel.pop().map(|(_, p)| p)
}
#[cfg(not(feature = "sqlite"))]
fn resolve_db(_data: &Path) -> Option<PathBuf> {
    None
}

impl Adapter for OpenCode {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn dependencies(&self, r: &SessionRef, probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if is_sqlite_source(r)? {
            return Some(());
        }
        let storage = r
            .path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "session"))
            .and_then(Path::parent)?;
        let messages = storage.join("message").join(&r.id);
        // Parts are keyed by the parsed message ID. Read only the ID (other fields are ignored),
        // and enumerate only this session's own message/part directories.
        for message in probe.list(&messages)?.iter() {
            #[derive(serde::Deserialize)]
            struct MessageId {
                #[serde(default)]
                id: String,
            }
            let file = fs::File::open(message).ok()?;
            let id: MessageId = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
            deps.directories.push(storage.join("part").join(id.id));
        }
        deps.directories.push(messages);
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.root
            .clone()
            .or_else(|| self.db.as_ref().and_then(|d| d.parent()).map(Path::to_path_buf))
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // The database first (it is the live store), then the frozen JSON tree for anything the
        // database doesn't have — a session present in both (the import left copies) appears once.
        let (mut out, seen) = self.discover_db();
        let Some(root) = &self.root else {
            return Ok(out);
        };
        let session_dir = root.join("session");
        for entry in WalkDir::new(&session_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("ses_") || !name.ends_with(".json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(name.trim_end_matches(".json"))
                .to_string();
            if seen.contains(&id) {
                continue;
            }
            let message_count = self.message_dir(&id).map(|d| count_turns(&d)).unwrap_or(0);
            out.push(SessionRef {
                id,
                harness: Harness::OpenCode,
                path: path.to_path_buf(),
                cwd: v.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| crate::ir::truncate(t, 80)),
                created_at: v.pointer("/time/created").and_then(super::ts_from_value),
                updated_at: v.pointer("/time/updated").and_then(super::ts_from_value),
                message_count,
            });
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::OpenCode, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            // A db-backed ref points at the `.db` file itself (like the other SQLite adapters).
            #[cfg(feature = "sqlite")]
            if self.db.as_ref() == Some(&r.path) || db::is_db_ref(r) {
                return db::stream(r, opts, sink);
            }
            // OpenCode stores a session as a *directory* of per-message JSON files (plus per-message
            // part files), each one document viewed by the record codec:
            //   1. read the small session meta file,
            //   2. order the message files by their `time/created` key alone,
            //   3. view each message file in order, then its part files one at a time, emit its IR
            //      message(s) to `sink`, and drop it before touching the next.
            let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
            let meta: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

            let mut s = Session {
                cwd: meta.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: meta.get("title").and_then(Value::as_str).map(str::to_string),
                created_at: meta.pointer("/time/created").and_then(super::ts_from_value),
                updated_at: meta.pointer("/time/updated").and_then(super::ts_from_value),
                source_path: Some(r.path.clone()),
                lineage: Lineage {
                    parent: meta
                        .get("parentID")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                    ..Lineage::default()
                },
                ..Session::new(r.id.clone(), Harness::OpenCode)
            };

            // Order message files by `time/created` (same ordering as before) while holding only the
            // sort key + path per file — never all message bodies at once.
            let mut order: Vec<(i64, PathBuf)> = Vec::new();
            if let Some(mdir) = self.message_dir(&r.id) {
                if let Ok(rd) = fs::read_dir(&mdir) {
                    for e in rd.filter_map(|e| e.ok()) {
                        let path = e.path();
                        let when = document(&path, |view, _| {
                            Ok(record::owned(record::get(view.get("time")?, "created")?)?
                                .and_then(|created| created.as_i64())
                                .unwrap_or(0))
                        })?;
                        if let Some(when) = when {
                            order.push((when, path));
                        }
                    }
                }
            }
            order.sort();

            sink.meta(&s);

            // All regenerated payloads share one private file for this session pass.
            let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena()?);

            // Re-read one message file at a time, emit, then drop before the next.
            for (_, path) in &order {
                let Some(mut record) = document(path, |view, source| {
                    record::Record::header(view, None, record::Ctx::new(opts.spans, source, &arena))
                })?
                .flatten() else {
                    continue;
                };
                if let Some(dir) = self.part_dir(record.id().unwrap_or("")) {
                    // Parts in file-name order, one file at a time.
                    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e.ok().map(|e| e.path()))
                        .collect();
                    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                    for path in paths {
                        document(&path, |view, source| {
                            record.part(view, None, record::Ctx::new(opts.spans, source, &arena))
                        })?;
                    }
                }
                for mut built in record.finish() {
                    fold_into_session(&mut s, &mut built)?;
                    if sink.message(built) == Flow::Stop {
                        return Ok(s);
                    }
                }
            }

            Ok(s)
        })
    }

    /// The todo list `todowrite` keeps in `opencode.db`'s `todo` table (1.x; 2.x has none).
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        #[cfg(feature = "sqlite")]
        if self.db.as_ref() == Some(&r.path) || db::is_db_ref(r) {
            return db::todos(r).into_iter().collect();
        }
        #[cfg(not(feature = "sqlite"))]
        let _ = r;
        Vec::new()
    }
}

/// Session facts a message reveals as it streams past: the session model (the first one seen when
/// the store gave none), the system prompt (a `system` record), and the IR diet — `Message::model`
/// stays set only when it differs from the session's.
fn fold_into_session(s: &mut Session, m: &mut Message) -> Result<()> {
    if s.model.is_none() {
        s.model = m.model.clone();
    }
    if m.model.is_some() && m.model == s.model {
        m.model = None;
    }
    if m.kind == MessageKind::SystemPrompt && s.system_prompt.is_none() {
        // System prompts are required as owned session metadata even for bounded readers.
        m.materialize(&s.resolver())?;
        s.system_prompt = m.text();
    }
    Ok(())
}

/// Count a session's conversational turns: message records whose `role` is `user`/`assistant`.
/// The [`SessionRef::message_count`] contract is user+assistant turns only — counting directory
/// entries (the old behavior) also picked up system/summary records and any stray files, so the
/// number disagreed with every other adapter. Only the role is selected; older headers can
/// contain large inline summaries. This never touches part files.
fn count_turns(dir: &std::path::Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .filter(|entry| {
            document(&entry.path(), |view, _| is_turn(view))
                .ok()
                .flatten()
                .unwrap_or(false)
        })
        .count()
}

/// A message record whose `role` is `user` or `assistant`.
fn is_turn(view: crate::json::JsonView<'_>) -> Result<bool> {
    Ok(matches!(
        record::string(view.get("role")?)?.as_deref(),
        Some("user" | "assistant")
    ))
}

/// One JSON document of the tree, validated and viewed once; None for a file that cannot be
/// captured or is not exactly one JSON value.
fn document<T>(
    path: &Path,
    f: impl for<'a> FnOnce(crate::json::JsonView<'a>, &crate::json::JsonSource) -> Result<T>,
) -> Result<Option<T>> {
    let Ok(source) = crate::json::JsonSource::capture(path) else {
        return Ok(None);
    };
    match source.read_syntax(|view| {
        if view.accepts_value()? {
            f(view, &source).map(Some)
        } else {
            Ok(None)
        }
    }) {
        Err(error) if error.is::<crate::json::InvalidJson>() => Ok(None),
        result => result,
    }
}

// ── the SQLite store ─────────────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
mod db {
    //! `opencode.db` — `session`, `message`, `part` (`packages/core/src/session/sql.ts`). `data`
    //! cells are viewed through [`Cells`] and decoded by the same record codec as the tree, with
    //! the row ids standing in for the ones the tree stored (as `message-v2.ts` hydrates them).
    use super::super::store::sqlite::{open_ro, Cells};
    use super::super::ts_from_epoch;
    use super::*;
    use rusqlite::types::ValueRef;
    use std::collections::HashMap;

    /// Session columns the IR first-classes (`parent_id` is `Lineage::parent`; everything else lands
    /// in `Session::extra["opencode"]` verbatim).
    const FIRSTCLASS: &[&str] = &[
        "id",
        "directory",
        "title",
        "time_created",
        "time_updated",
        "parent_id",
        "fork_session_id",
    ];
    /// Columns opencode declares `{ mode: "json" }` — stored as JSON text, decoded on read.
    const JSON_COLS: &[&str] = &[
        "model",
        "metadata",
        "revert",
        "permission",
        "summary_diffs",
        "fork_boundary",
    ];

    /// The session's `todo` rows in position order, as its plan. `None` when the table is absent
    /// (2.x) or holds nothing for the session.
    pub(super) fn todos(r: &SessionRef) -> Option<crate::affordance::Affordance> {
        let conn = open_ro(&r.path).ok()?;
        conn.read(|conn| {
            if !has_table(conn, "todo") {
                return Ok(None);
            }
            let mut st = conn.prepare("SELECT content, status FROM todo WHERE session_id = ?1 ORDER BY position")?;
            let items: Vec<crate::affordance::Item> = st
                .query_map([&r.id], |row| {
                    Ok(crate::affordance::Item {
                        content: row.get(0)?,
                        status: row.get(1)?,
                    })
                })?
                .filter_map(|x| x.ok())
                .collect();
            Ok(crate::affordance::plan_state(
                crate::affordance::Kind::Plan,
                "todo",
                items,
            ))
        })
        .ok()
        .flatten()
    }

    pub(super) fn is_db_ref(r: &SessionRef) -> bool {
        r.path.extension().and_then(|e| e.to_str()) == Some("db")
    }

    fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// One [`SessionRef`] per session, newest first: the 2.x `session_v2` rows, then the 1.x
    /// `session` rows they do not already cover (a migrated database holds both under the same id;
    /// 2.x writes only the new tables, so they win). Either table set may be absent: a 1.x database
    /// has no `session_v2`, a database 2.x created has no `session`.
    pub(super) fn discover(db: &Path) -> Result<Vec<SessionRef>> {
        crate::harness::store::sqlite::cached_discover(db, || {
            let conn = open_ro(db)?;
            conn.read(|reader| discover_snapshot(db, reader.reads(), reader))
        })
    }

    fn discover_snapshot(
        db: &Path,
        reads: &crate::source::ReadSet,
        conn: &rusqlite::Connection,
    ) -> Result<Vec<SessionRef>> {
        let _transaction = conn.unchecked_transaction()?;
        let mut refs = if has_table(conn, "session_v2") && has_table(conn, "session_message") {
            discover_v2(conn, db)?
        } else {
            Vec::new()
        };
        if has_table(conn, "session") && has_table(conn, "message") {
            let seen: HashSet<String> = refs.iter().map(|r| r.id.clone()).collect();
            refs.extend(discover_v1(conn, db, reads, &seen)?);
        }
        Ok(refs)
    }

    fn discover_v2(conn: &rusqlite::Connection, db: &Path) -> Result<Vec<SessionRef>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT session_id, COUNT(*) FROM session_message WHERE type IN ('user', 'assistant') GROUP BY session_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for (sid, n) in rows.flatten() {
                counts.insert(sid, n.max(0) as usize);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, directory, title, time_created, time_updated FROM session_v2 ORDER BY time_updated DESC",
        )?;
        let refs = stmt
            .query_map([], |r| session_ref(db, r))?
            .flatten()
            .map(|mut r| {
                r.message_count = counts.get(&r.id).copied().unwrap_or(0);
                r
            })
            .collect();
        Ok(refs)
    }

    fn session_ref(db: &Path, r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRef> {
        Ok(SessionRef {
            id: r.get(0)?,
            harness: Harness::OpenCode,
            path: db.to_path_buf(),
            cwd: r
                .get::<_, Option<String>>(1)?
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            title: r
                .get::<_, Option<String>>(2)?
                .filter(|t| !t.is_empty())
                .map(|t| crate::ir::truncate(&t, 80)),
            created_at: r.get::<_, Option<i64>>(3)?.and_then(ts_from_epoch),
            updated_at: r.get::<_, Option<i64>>(4)?.and_then(ts_from_epoch),
            message_count: 0,
        })
    }

    /// Count the turns the stream reads: `message` cells whose final `role` is user or assistant.
    fn discover_v1(
        conn: &rusqlite::Connection,
        db: &Path,
        reads: &crate::source::ReadSet,
        seen: &HashSet<String>,
    ) -> Result<Vec<SessionRef>> {
        // Migrated stores retain old message bodies. Select only surviving legacy sessions
        // before touching those bodies; a fully migrated store needs no legacy message scan.
        let mut refs: Vec<SessionRef> = conn
            .prepare("SELECT id, directory, title, time_created, time_updated FROM session ORDER BY time_updated DESC")?
            .query_map([], |r| session_ref(db, r))?
            .flatten()
            .filter(|reference| !seen.contains(&reference.id))
            .collect();
        if refs.is_empty() {
            return Ok(refs);
        }
        let wanted: HashSet<&str> = refs.iter().map(|reference| reference.id.as_str()).collect();
        let mut counts: HashMap<String, usize> = HashMap::new();
        let cells = Cells::new(conn, db, reads, "message")?;
        let mut stmt = conn.prepare(&format!("SELECT session_id, {} FROM message", cells.select("data")))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let Ok(sid) = row.get::<_, String>(0) else { continue };
            if !wanted.contains(sid.as_str()) {
                continue;
            }
            let Some(cell) = cells.cell(conn, "message", "data", row, 1)? else {
                continue;
            };
            if cells.read(cell, |view| Ok(view.is_object()? && is_turn(view)?))? == Some(true) {
                *counts.entry(sid).or_default() += 1;
            }
        }
        for reference in &mut refs {
            reference.message_count = counts.get(&reference.id).copied().unwrap_or(0);
        }
        Ok(refs)
    }

    /// One row as a JSON object keyed by column name (`NULL`s dropped, JSON columns decoded).
    fn row_to_map(cols: &[String], row: &rusqlite::Row<'_>) -> Result<Map<String, Value>> {
        let mut m = Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                ValueRef::Null => continue,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => {
                    let t = String::from_utf8_lossy(t).into_owned();
                    if JSON_COLS.contains(&c.as_str()) {
                        serde_json::from_str(&t).unwrap_or(Value::String(t))
                    } else {
                        Value::String(t)
                    }
                }
                ValueRef::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            };
            m.insert(c.clone(), v);
        }
        Ok(m)
    }

    /// Stream one db-backed session: the `session` row → [`Session`] metadata + `extra`, then each
    /// `message` row with its `part` rows hydrated and mapped one message at a time.
    pub(super) fn stream(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        let conn = open_ro(&r.path)?;
        conn.read(|reader| stream_snapshot(r, opts, sink, reader.reads(), reader))
    }

    fn stream_snapshot(
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        reads: &crate::source::ReadSet,
        conn: &rusqlite::Connection,
    ) -> Result<Session> {
        let _transaction = conn.unchecked_transaction()?;
        // The 2.x tables win for a session in both (a migrated database; module doc of `v2`).
        let v2 = has_table(conn, "session_v2")
            && conn
                .query_row(
                    "SELECT 1 FROM session_v2 WHERE id = ?1",
                    rusqlite::params![r.id],
                    |_| Ok(()),
                )
                .is_ok();
        let table = if v2 { "session_v2" } else { "session" };
        let row = {
            // `SELECT *` + names, so a column that only exists in newer schemas (`metadata`,
            // 2026-05; `workspace_id`, `path`, …) is read when present and never demanded.
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))?;
            let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = stmt.query(rusqlite::params![r.id])?;
            match rows.next()? {
                Some(row) => row_to_map(&cols, row)?,
                None => anyhow::bail!("no opencode session {} in {}", r.id, r.path.display()),
            }
        };
        let mut bag = Map::new();
        for (k, v) in &row {
            // Keep the session's own facts (agent, model, cost, tokens_*, summary_*, share_url,
            // version, project_id, time_archived, metadata, …) verbatim; skip blanks.
            if FIRSTCLASS.contains(&k.as_str()) || v.as_str().is_some_and(str::is_empty) {
                continue;
            }
            bag.insert(k.clone(), v.clone());
        }
        let mut s = Session {
            // A sub-agent session (`@agent` mention, `subtask`) points at the session that spawned it.
            lineage: Lineage {
                parent: row
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                // 2.x `fork_session_id`: the session `--fork` copied this one from.
                forked_from: row
                    .get("fork_session_id")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                ..Lineage::default()
            },
            cwd: row
                .get("directory")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            title: row
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            created_at: row.get("time_created").and_then(super::super::ts_from_value),
            updated_at: row.get("time_updated").and_then(super::super::ts_from_value),
            // `session.model` = `{id, providerID, variant?}`; the first message's model is the
            // fallback for rows written before the column existed.
            model: row
                .get("model")
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            source_path: Some(r.path.clone()),
            ..Session::new(r.id.clone(), Harness::OpenCode)
        };
        if !bag.is_empty() {
            s.harness_extra_mut(Harness::OpenCode).extend(bag);
        }
        sink.meta(&s);
        if v2 {
            return stream_v2(
                conn,
                s,
                opts,
                sink,
                &Cells::new(conn, &r.path, reads, "session_message")?,
            );
        }
        // Each `message` cell with its `part` cells (ordered by id), one record at a time.
        let messages = Cells::new(conn, &r.path, reads, "message")?;
        let parts = Cells::new(conn, &r.path, reads, "part")?;
        fn ctx<'a>(spans: bool, cells: &'a Cells) -> record::Ctx<'a> {
            let (source, arena) = cells.out();
            record::Ctx::new(spans, source, arena)
        }
        let mut message_rows = conn.prepare(&format!(
            "SELECT id, {} FROM message WHERE session_id = ?1 ORDER BY time_created, id",
            messages.select("data")
        ))?;
        let mut part_rows = conn.prepare(&format!(
            "SELECT id, {} FROM part WHERE message_id = ?1 ORDER BY id",
            parts.select("data")
        ))?;
        let mut rows = message_rows.query(rusqlite::params![r.id])?;
        while let Some(row) = rows.next()? {
            let mid: String = row.get(0)?;
            let Some(cell) = messages.cell(conn, "message", "data", row, 1)? else {
                continue;
            };
            let Some(Some(mut record)) = messages.read(cell, |view| {
                record::Record::header(view, Some(&mid), ctx(opts.spans, &messages))
            })?
            else {
                continue;
            };
            let mut part_rows = part_rows.query(rusqlite::params![mid])?;
            while let Some(row) = part_rows.next()? {
                let Ok(pid) = row.get::<_, String>(0) else {
                    continue;
                };
                if let Some(cell) = parts.cell(conn, "part", "data", row, 1)? {
                    parts.read(cell, |view| {
                        record.part(view, Some([&pid, &mid, &r.id]), ctx(opts.spans, &parts))
                    })?;
                }
            }
            for mut built in record.finish() {
                fold_into_session(&mut s, &mut built)?;
                if sink.message(built) == Flow::Stop {
                    return Ok(s);
                }
            }
        }
        Ok(s)
    }

    /// The 2.x `session_message` rows of one session, in `seq` order (module doc of `v2`). A cell
    /// that is not one JSON value is a skipped line.
    fn stream_v2(
        conn: &rusqlite::Connection,
        mut s: Session,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        cells: &Cells,
    ) -> Result<Session> {
        let (source, arena) = cells.out();
        let ctx = record::Ctx::new(opts.spans, source, arena);
        let mut stmt = conn.prepare(&format!(
            "SELECT id, type, {} FROM session_message WHERE session_id = ?1 ORDER BY seq",
            cells.select("data")
        ))?;
        let mut rows = stmt.query(rusqlite::params![s.id])?;
        let mut unknown: std::collections::BTreeMap<String, u64> = Default::default();
        let mut unreadable = 0u64;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let ty: String = row.get(1)?;
            if opts.complete && !v2::known_type(&ty) {
                *unknown.entry(ty.clone()).or_default() += 1;
            }
            let decoded = match cells.cell(conn, "session_message", "data", row, 2)? {
                Some(cell) => cells.read(cell, |view| {
                    let Some(mut built) = v2::build(&id, &ty, view, opts, ctx)? else {
                        return Ok(None);
                    };
                    if opts.complete {
                        v2::attach_complete(&mut built, &id, &ty, view.argument(opts.spans)?);
                    }
                    Ok(Some(built))
                })?,
                None => None,
            };
            let mut built = match decoded {
                Some(Some(built)) => built,
                Some(None) => {
                    *unknown.entry(ty).or_default() += 1;
                    Vec::new()
                }
                None => {
                    unreadable += 1;
                    continue;
                }
            };
            if let Some(record) = built.first_mut().and_then(|message| message.complete_record.as_mut()) {
                let mut model = s.model.clone();
                let record = std::sync::Arc::make_mut(record);
                for original in &mut record.projection {
                    if model.is_none() {
                        model = original.model.clone();
                    }
                    if original.model.is_some() && original.model == model {
                        original.model = None;
                    }
                }
            }
            for mut m in built {
                fold_into_session(&mut s, &mut m)?;
                if sink.message(m) == Flow::Stop {
                    return Ok(s);
                }
            }
        }
        if !unknown.is_empty() {
            s.harness_extra_mut(Harness::OpenCode)
                .insert("unknown_message_types".into(), json!(unknown));
        }
        super::super::note_skipped_lines(&mut s, unreadable);
        Ok(s)
    }
    #[cfg(test)]
    mod discovery_tests {
        use super::*;

        unsafe extern "C" fn deny_legacy_message_read(
            _: *mut std::ffi::c_void,
            action: i32,
            table: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
        ) -> i32 {
            if action == rusqlite::ffi::SQLITE_READ && !table.is_null()
                // SAFETY: SQLite supplies a live nul-terminated table name during this call.
                && unsafe { std::ffi::CStr::from_ptr(table) }.to_bytes() == b"message"
            {
                rusqlite::ffi::SQLITE_DENY
            } else {
                rusqlite::ffi::SQLITE_OK
            }
        }

        #[test]
        fn migrated_discovery_never_scans_shadowed_legacy_messages() -> Result<()> {
            let world = cv_testkit::World::new("opencode-shadowed-discovery");
            let path = world.base.join("provenance");
            fs::write(&path, b"unchanged")?;
            for count in [1, 16, 256] {
                let conn = rusqlite::Connection::open_in_memory()?;
                conn.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT); INSERT INTO session VALUES('ses_m','/old','old title',1,1); INSERT INTO session_v2 VALUES('ses_m','/new','current title',2,2);")?;
                let payload = json!({"role":"user","body":"legacy body".repeat(128)}).to_string();
                for index in 0..count {
                    conn.execute(
                        "INSERT INTO message VALUES(?1,'ses_m',?2)",
                        rusqlite::params![format!("msg_{index}"), payload],
                    )?;
                }
                // Denying any legacy message read makes this an algorithmic control: growing
                // old histories must not even prepare a scan when every session is covered.
                // SAFETY: this static callback owns no context and lives until conn drops.
                unsafe {
                    rusqlite::ffi::sqlite3_set_authorizer(
                        conn.handle(),
                        Some(deny_legacy_message_read),
                        std::ptr::null_mut(),
                    );
                }
                let refs = discover_snapshot(&path, &Default::default(), &conn)?;
                assert_eq!(refs.len(), 1);
                assert_eq!(refs[0].title.as_deref(), Some("current title"));
                assert_eq!(refs[0].message_count, 0);
            }
            Ok(())
        }

        #[test]
        fn mixed_store_keeps_unmigrated_counts_and_order() -> Result<()> {
            let world = cv_testkit::World::new("opencode-mixed-discovery");
            let path = world.base.join("provenance");
            fs::write(&path, b"unchanged")?;
            let conn = rusqlite::Connection::open_in_memory()?;
            conn.execute_batch(r#"CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT); INSERT INTO session VALUES('covered','/old','old',1,3),('older','/old','older',1,1),('newer','/old','newer',1,2); INSERT INTO session_v2 VALUES('covered','/new','current',2,2); INSERT INTO message VALUES('hidden','covered','{broken'),('orphan','missing','{broken'),('old','older','{"role":"user"}'),('new','newer','{"role":"system","role":"assistant"}');"#)?;
            let refs = discover_snapshot(&path, &Default::default(), &conn)?;
            assert_eq!(
                refs.iter()
                    .map(|r| (r.id.as_str(), r.message_count))
                    .collect::<Vec<_>>(),
                vec![("covered", 0), ("newer", 1), ("older", 1)]
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    fn parts(json_strs: &[&str]) -> Vec<Value> {
        json_strs.iter().map(|s| serde_json::from_str(s).unwrap()).collect()
    }

    /// The record codec over owned views, everything inline.
    fn build_messages(mv: &Value, parts: &[Value]) -> Vec<Message> {
        let ctx = record::Ctx::Owned;
        let mut record = record::Record::header(crate::json::JsonView::owned(mv), None, ctx)
            .unwrap()
            .unwrap();
        for part in parts {
            record.part(crate::json::JsonView::owned(part), None, ctx).unwrap();
        }
        record.finish()
    }

    /// Decode the `data` document at `path` both ways, everything inline and by the span rule,
    /// and return them with the spans one materialized: the demand never changes content.
    fn both_demands(
        path: &Path,
        decode: impl Fn(crate::json::JsonView<'_>, record::Ctx<'_>) -> Result<Vec<Message>>,
    ) -> (Vec<Message>, Vec<Message>) {
        let source = crate::json::JsonSource::capture(path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let owned = source.read_document(|view| decode(view, record::Ctx::Owned)).unwrap();
        let mut spans = source
            .read_document(|view| decode(view, record::Ctx::new(true, &source, &arena)))
            .unwrap();
        for message in &mut spans {
            message.materialize(&crate::lazy::Resolver::new(None)).unwrap();
        }
        (owned, spans)
    }

    #[test]
    fn parts_generation_text_reasoning_tool_split() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"msg_1","role":"assistant","modelID":"claude","agent":"build",
                "providerID":"anthropic","cost":0.01,"finish":"tool-calls",
                "time":{"created":1767000000000},
                "tokens":{"input":10,"output":5,"reasoning":3,"cache":{"read":1,"write":2}}}"#,
        )
        .unwrap();
        let p = parts(&[
            r#"{"type":"reasoning","text":"thinking...","metadata":{"anthropic":{"signature":"SIG"}},"time":{"start":1}}"#,
            r#"{"type":"text","text":"Hello"}"#,
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"List","metadata":{"exit":0}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        // assistant message + a tool-role message
        assert_eq!(out.len(), 2);
        let asst = &out[0];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!(asst.model.as_deref(), Some("claude"));
        // thinking signature preserved
        match &asst.content[0] {
            Block::Thinking { text, signature, .. } => {
                assert_eq!(text, "thinking...");
                assert_eq!(signature.as_deref(), Some("SIG"));
            }
            b => panic!("expected thinking, got {b:?}"),
        }
        assert!(matches!(&asst.content[1], Block::Text { text } if text == "Hello"));
        assert!(matches!(&asst.content[2], Block::ToolUse { name, .. } if name == "bash"));
        assert_eq!((asst.kind, asst.origin), (MessageKind::Reply, Origin::Model));
        // message-level facts live in the harness bag only
        let bag = &asst.extra["opencode"];
        assert_eq!(bag["agent"], "build");
        assert_eq!(bag["finish"], "tool-calls");
        assert_eq!(bag["providerID"], "anthropic");
        assert!(
            bag.get("cost").is_none() && bag.get("reasoning_tokens").is_none(),
            "{bag}"
        );
        assert_eq!(asst.extra.len(), 1, "{:?}", asst.extra);
        // usage, cost and reasoning first-class
        let u = asst.usage.as_ref().unwrap();
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.cache_creation_tokens, Some(2));
        assert_eq!(u.reasoning_tokens, Some(3));
        assert_eq!(u.cost_usd, Some(0.01));
        // tool result became its own Tool message
        assert_eq!(out[1].role, Role::Tool);
        assert_eq!((out[1].kind, out[1].origin), (MessageKind::ToolResult, Origin::Harness));
        match &out[1].content[0] {
            Block::ToolResult {
                content,
                is_error,
                tool_use_id,
                tool_name,
                status,
                ..
            } => {
                assert_eq!(content, "a\nb");
                assert!(!is_error);
                assert_eq!(tool_use_id, "c1");
                assert_eq!(tool_name.as_deref(), Some("bash"));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            b => panic!("expected tool result, got {b:?}"),
        }
    }

    #[test]
    fn tool_error_state_marks_is_error() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c","tool":"edit","state":{"status":"error","input":{},"error":"oldString not found","time":{"start":1,"end":2}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        let res = out.iter().flat_map(|m| &m.content).find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.to_string(), *is_error)),
            _ => None,
        });
        assert_eq!(res, Some(("oldString not found".to_string(), true)));
    }

    #[test]
    fn file_part_image_vs_reference() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"file","mime":"image/png","url":"file:///x.png","filename":"x.png"}"#,
            r#"{"type":"file","mime":"text/plain","url":"file:///Makefile","filename":"Makefile","source":{"type":"file","path":"Makefile"}}"#,
            r#"{"type":"file","mime":"application/x-directory","url":"file:///tools/","filename":"tools/"}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        // image -> Image block
        assert!(matches!(&m.content[0], Block::Image { media_type, .. } if media_type.as_deref()==Some("image/png")));
        // text file -> File reference, NOT an image
        assert!(matches!(&m.content[1], Block::File { path, .. } if path.as_deref()==Some("Makefile")));
        // directory -> File reference, NOT an image
        assert!(
            matches!(&m.content[2], Block::File { source, .. } if source.as_deref().unwrap_or("").contains("tools/"))
        );
    }

    #[test]
    fn agent_subtask_patch_snapshot_steps_preserved() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"agent","name":"oracle","source":{"value":"@oracle","start":0,"end":7}}"#,
            r#"{"type":"subtask","prompt":"do the thing","description":"d","agent":"build"}"#,
            r#"{"type":"patch","hash":"abc","files":["/a.rs"]}"#,
            r#"{"type":"snapshot","snapshot":"snap123"}"#,
            r#"{"type":"step-start"}"#,
            r#"{"type":"step-finish","reason":"tool-calls","cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
            r#"{"type":"retry","attempt":1,"error":{"name":"APIError"},"time":{"created":9}}"#,
            r#"{"type":"compaction","auto":false}"#,
            r#"{"type":"galaxy-brain-future-part","wat":true}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        let bag = &m.extra["opencode"];
        let len = |k: &str| bag.get(k).and_then(|v| v.as_array()).map(|a| a.len());
        assert_eq!(len("patches"), Some(1));
        assert_eq!(len("snapshots"), Some(1));
        assert_eq!(len("steps"), Some(2), "step-start/finish never surface as messages");
        assert_eq!(len("retries"), Some(1));
        assert_eq!(len("compactions"), Some(1));
        assert_eq!(len("agent_refs"), Some(1));
        assert_eq!(len("subtasks"), Some(1));
        // unknown part type is never dropped
        assert_eq!(len("unknown_parts"), Some(1));
        assert_eq!(m.extra.len(), 1, "nothing flat: {:?}", m.extra);
        // the compaction part outranks the spawn parts for the kind (it is the rarer, structural one)
        assert_eq!((m.kind, m.origin), (MessageKind::CompactionBoundary, Origin::Harness));
        // without it, the agent mention / subtask make the record a spawn
        let out = build_messages(&mv, &p[..2]);
        assert_eq!(
            (out[0].kind, out[0].origin),
            (MessageKind::SubagentSpawn, Origin::Model)
        );
        // agent + subtask also surface as text
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text == "@oracle")));
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("subtask → build"))));
    }

    #[test]
    fn inline_summary_generation_fallback() {
        // Older generation: no parts, inline summary object with diffs.
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "summary":{"title":"Cleanup","body":"Removed debug comments.",
                "diffs":[{"file":"a.rs","before":"x","after":"y"}]}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text == "Removed debug comments."));
        assert_eq!((out[0].kind, out[0].origin), (MessageKind::Prompt, Origin::Human));
        // diffs preserved in the bag
        assert!(out[0].extra["opencode"].get("summary_diffs").is_some());
    }

    #[test]
    fn compaction_is_a_user_boundary_then_an_assistant_summary() {
        // As opencode writes it: the user-side request is one record with a lone `compaction`
        // part; the summary is an assistant record flagged `summary: true` (agent `compaction`).
        let req: Value =
            serde_json::from_str(r#"{"id":"u","role":"user","time":{"created":1},"agent":"Sisyphus"}"#).unwrap();
        let out = build_messages(&req, &parts(&[r#"{"type":"compaction","auto":true}"#]));
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(
            (b.role, b.kind, b.origin),
            (Role::User, MessageKind::CompactionBoundary, Origin::Harness)
        );
        assert!(b.content.is_empty(), "the boundary has no text of its own");
        assert_eq!(b.extra["opencode"]["compactions"][0]["auto"], true);

        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":2},"agent":"compaction","summary":true}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"compacted history…"}"#]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::Assistant, MessageKind::CompactionSummary, Origin::Harness)
        );
        assert!(
            m.extra["opencode"].get("is_compaction_summary").is_none(),
            "the kind says it"
        );
        // the boolean must NOT be rendered as a text body; the real text comes from parts
        assert!(matches!(&m.content[0], Block::Text { text } if text == "compacted history…"));
    }

    #[test]
    fn inline_summary_body_wins_then_title_and_parts_win_over_both() {
        let world = crate::test_world("opencode-summary");
        let path = world.base.join("message.json");
        let body = "summary\n\"text\" 🌟".repeat(9000);
        let title = "title".repeat(9000);
        for (summary, expected) in [
            (
                json!({"body":body,"title":title,"diffs":[{"file":"a.rs"}]}),
                Some(&body),
            ),
            (json!({"title":title}), Some(&title)),
            (json!({"body":"","title":title}), None),
            (json!({"body":null,"title":title}), None),
            (Value::Bool(true), None),
        ] {
            let value = json!({"id":"m","role":"assistant","time":{"created":2},"summary":summary,"ignored":body});
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            for parts in [vec![], vec![json!({"type":"text","text":"part wins"})]] {
                let (owned, spans) = both_demands(&path, |view, ctx| {
                    let mut record = record::Record::header(view, None, ctx)?.unwrap();
                    for part in &parts {
                        record.part(crate::json::JsonView::owned(part), None, ctx)?;
                    }
                    Ok(record.finish())
                });
                assert_eq!(
                    serde_json::to_value(&owned).unwrap(),
                    serde_json::to_value(&spans).unwrap()
                );
                let text = owned.first().and_then(Message::text);
                match (parts.is_empty(), expected) {
                    (true, Some(expected)) => assert_eq!(text.as_ref(), Some(expected)),
                    (true, None) => assert_eq!(text, None),
                    (false, _) => assert_eq!(text.as_deref(), Some("part wins")),
                }
            }
        }
        // Long summary text stays in its file, guarded.
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"id":"m","role":"user","summary":{"body":body}})).unwrap(),
        )
        .unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let mut messages = source
            .read_document(|view| {
                Ok(
                    record::Record::header(view, None, record::Ctx::new(true, &source, &arena))?
                        .unwrap()
                        .finish(),
                )
            })
            .unwrap();
        std::fs::write(&path, b"{}").unwrap();
        assert!(messages[0].materialize(&crate::lazy::Resolver::new(None)).is_err());
    }

    #[test]
    fn long_part_payloads_resolve_the_same_in_every_demand() {
        let world = crate::test_world("opencode-parts");
        let path = world.base.join("parts.json");
        let payload = "line\n\"quoted\"\t\\tail".repeat(9000);
        let parts = json!([
            {"type":"text", "text":payload},
            {"type":"reasoning", "text":payload, "metadata":{"anthropic":{"signature":"SIG"}}},
            {"type":"reasoning", "text":"", "metadata":{"anthropic":{"signature":"ONLY"}}},
            {"type":"tool", "callID":"one", "tool":"read", "state":{"status":"completed", "input":{"payload":payload}, "output":payload, "metadata":{"exit":0}}},
            {"type":"tool", "callID":"two", "tool":"read", "state":{"status":"error", "input":null, "error":{"message":"structured"}, "output":payload}},
            {"type":"tool", "callID":"three", "tool":"read", "state":{"status":"error", "input":true, "error":payload}},
            {"type":"tool", "callID":"four", "tool":"read", "state":{"status":"completed", "input":null, "output":{"payload":payload}}},
            {"type":"future", "payload":"preserved"}
        ]);
        std::fs::write(&path, serde_json::to_vec(&parts).unwrap()).unwrap();
        let mv = json!({"id":"message", "role":"assistant", "modelID":"model", "tokens":{"input":1}});
        let (owned, spans) = both_demands(&path, |view, ctx| {
            let mut record = record::Record::header(crate::json::JsonView::owned(&mv), None, ctx)?.unwrap();
            for part in view.array()?.unwrap() {
                record.part(part?, None, ctx)?;
            }
            Ok(record.finish())
        });
        assert_eq!(
            serde_json::to_value(&owned).unwrap(),
            serde_json::to_value(&spans).unwrap()
        );
        let Block::ToolResult { content, .. } = &owned[1].content[1] else {
            panic!("structured error result")
        };
        assert_eq!(content, r#"{"message":"structured"}"#);
        let Block::ToolResult { content, .. } = &owned[1].content[3] else {
            panic!("object output")
        };
        assert_eq!(
            content.inline_str(),
            Some(serde_json::to_string(&json!({"payload":payload})).unwrap().as_str())
        );
        assert_eq!(owned[0].extra["opencode"]["unknown_parts"][0]["payload"], "preserved");
    }

    #[test]
    fn v2_shell_command_output_priority_and_canonical_fallback() {
        let world = crate::test_world("opencode-shell");
        let path = world.base.join("shell.json");
        let body = "escaped\n\" λ🌙".repeat(6000);
        let quoted = serde_json::to_string(&body).unwrap();
        let canonical = |output: &str| serde_json::to_string(&serde_json::from_str::<Value>(output).unwrap()).unwrap();
        let cases = [
            (
                format!(
                    r#"{{"command":{quoted},"output":{{"output":{quoted},"cursor":1,"size":3,"truncated":false}}}}"#
                ),
                Some(format!("$ {body}\n{body}")),
            ),
            (
                format!(r#"{{"command":"echo hi","output":{quoted}}}"#),
                Some(format!("$ echo hi\n{body}")),
            ),
            (
                format!(r#"{{"command":"echo hi","output":{{"text":{quoted}}}}}"#),
                Some(format!("$ echo hi\n{body}")),
            ),
            // Presence wins before type: a non-string inner output renders the whole object.
            (
                format!(r#"{{"output":{{"output":null,"text":{quoted},"z":2,"a":1}}}}"#),
                Some(canonical(&format!(r#"{{"output":null,"text":{quoted},"z":2,"a":1}}"#))),
            ),
            (
                format!(r#"{{"output":{{"output":false,"text":{quoted}}}}}"#),
                Some(canonical(&format!(r#"{{"output":false,"text":{quoted}}}"#))),
            ),
            (format!(r#"{{"output":{{"output":"","text":{quoted}}}}}"#), None),
            (
                format!(
                    r#"{{"command":"first","command":{quoted},"output":{{"output":"first","outpu\u0074":{quoted}}}}}"#
                ),
                Some(format!("$ {body}\n{body}")),
            ),
            (
                format!(r#"{{"output":{{"z":{quoted},"a":1,"z":"duplicate winner"}}}}"#),
                Some(r#"{"z":"duplicate winner","a":1}"#.into()),
            ),
            (r#"{"command":"","output":null}"#.into(), None),
            (r#"{"command":null,"output":false}"#.into(), Some("false".into())),
            (r#"{"output":1.00}"#.into(), Some("1.0".into())),
            (
                r#"{"output":[1,"text",null]}"#.into(),
                Some(r#"[1,"text",null]"#.into()),
            ),
            (r#"{"output":{}}"#.into(), Some("{}".into())),
        ];
        for (raw, expected) in cases {
            let wire = format!(
                "{{\"time\":{{\"created\":1}},\"metadata\":{{\"source\":\"shell\",\"background\":true}},{}",
                &raw[1..]
            );
            fs::write(&path, &wire).unwrap();
            let (owned, spans) = both_demands(&path, |view, ctx| {
                Ok(v2::build("message", "shell", view, &ParseOptions::complete(), ctx)?.unwrap())
            });
            assert_eq!(serde_json::to_vec(&owned).unwrap(), serde_json::to_vec(&spans).unwrap());
            assert_eq!(owned[0].text(), expected, "{}", &wire[..wire.len().min(200)]);
            assert_eq!(owned[0].extra["opencode"]["metadata"]["background"], true);
        }
        // A long prefixed command is generated text held in the arena, guarded by the source.
        fs::write(&path, format!(r#"{{"command":{quoted}}}"#)).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let messages = source
            .read_document(|view| {
                Ok(v2::build(
                    "message",
                    "shell",
                    view,
                    &ParseOptions::lazy(),
                    record::Ctx::new(true, &source, &arena),
                )?
                .unwrap())
            })
            .unwrap();
        assert!(matches!(
            &messages[0].content[0],
            Block::Text {
                text: crate::Text::Source(_)
            }
        ));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_v2_shell_projection_preserves_modes_corpus_and_source_errors() {
        let world = crate::test_world("opencode-shell-db");
        let db = world.base.join("opencode.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,data TEXT); INSERT INTO session_v2 VALUES('session','/synthetic','shell',1,1);").unwrap();
        let body = "escaped\nλ🌙".repeat(5000);
        let value = json!({"time":{"created":1},"shellID":"shell","command":"echo user-shell","status":"exited","exit":0,"output":{"output":body,"cursor":3,"size":3,"truncated":false},"metadata":{"background":true}});
        connection
            .execute(
                "INSERT INTO session_message VALUES('message','session','shell',1,?1)",
                [value.to_string()],
            )
            .unwrap();
        let adapter = OpenCode::with_db(db.clone());
        let reference = SessionRef::new("session", Harness::OpenCode, &db);
        for opts in [
            ParseOptions::lazy(),
            ParseOptions::facts(),
            ParseOptions {
                spans: true,
                ..ParseOptions::complete()
            },
        ] {
            let mut actual = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
            assert!(matches!(
                actual.messages[0].content[1],
                Block::Text {
                    text: crate::Text::Source(_)
                }
            ));
            actual.materialize().unwrap();
            // Complete demand now retains the actual native record. Compare against the
            // same owned demand, rather than the bulk projection that omits rich metadata.
            let owned_opts = ParseOptions { spans: false, ..opts };
            let owned = crate::stream::collect_with(&adapter, &reference, &owned_opts).unwrap();
            let actual = serde_json::to_vec(&actual).unwrap();
            let expected = serde_json::to_vec(&owned).unwrap();
            let mismatch = actual
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(actual.len().min(expected.len()));
            let context = |bytes: &[u8]| {
                String::from_utf8_lossy(&bytes[mismatch.saturating_sub(60)..bytes.len().min(mismatch + 120)])
                    .into_owned()
            };
            assert!(
                actual == expected,
                "serialized mismatch at byte {mismatch}: actual length {}, owned length {}; actual {:?}; owned {:?}",
                actual.len(),
                expected.len(),
                context(&actual),
                context(&expected)
            );
        }
        let mut retained = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
        connection.execute("UPDATE session_message SET data='{}'", []).unwrap();
        assert!(retained.materialize().is_err());
        connection
            .execute("UPDATE session_message SET data=?1", [value.to_string()])
            .unwrap();
        let mut sink = crate::stream::TryFnSink::new(|_| {
            connection.execute("UPDATE session_message SET data='{}'", []).unwrap();
            anyhow::bail!("consumer cap")
        });
        let error = adapter
            .stream(&reference, &ParseOptions::lazy(), &mut sink)
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed"), "{error:#}");
        assert!(!format!("{error:#}").contains("consumer cap"));
        // The current native user-shell record is part of the committed corpus, not a legacy edge.
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db");
        if !corpus.exists() {
            return; // The published crate excludes the real harness corpus.
        }
        let corpus_adapter = OpenCode::with_db(corpus.clone());
        // Locate its containing session read-only; fixture IDs should not become codec knowledge.
        let reader = crate::harness::store::sqlite::open_ro(&corpus).unwrap();
        let id: String = reader
            .query_row(
                "SELECT session_id FROM session_message WHERE type='shell' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let corpus_reference = SessionRef::new(id, Harness::OpenCode, &corpus);
        let expected = crate::stream::collect_with(&corpus_adapter, &corpus_reference, &ParseOptions::bulk()).unwrap();
        let mut actual =
            crate::stream::collect_with(&corpus_adapter, &corpus_reference, &ParseOptions::lazy()).unwrap();
        actual.materialize().unwrap();
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }

    #[test]
    fn v2_tools_keep_join_order_and_scalar_arguments() {
        let world = crate::test_world("opencode-v2-tools");
        let path = world.base.join("content.json");
        let payload = "escaped\n\"text\"".repeat(9000);
        let content = json!([
            {"type":"text", "text":payload},
            {"type":"reasoning", "text":payload, "state":{"signature":"SIG"}},
            {"type":"tool", "id":"one", "name":"read", "state":{"status":"completed", "input":{"payload":payload}, "content":[{"type":"text", "text":payload},{"type":"file", "mime":"text/plain", "url":"file:///x"},{"type":"text", "text":""},{"future":payload}]}},
            {"type":"tool", "id":"two", "name":"read", "state":{"status":"completed", "input":"42", "content":[]}},
            {"type":"tool", "id":"three", "name":"read", "state":{"status":"error", "input":"", "error":{"message":"failure"}}},
            {"type":"tool", "id":"four", "name":"read", "state":{"status":"running", "input":serde_json::to_string(&json!({"payload":payload})).unwrap()}},
            {"type":"tool", "id":"five", "name":"read", "state":{"status":"running", "input":format!("true{}", " ".repeat(50000))}},
            {"type":"tool", "id":"six", "name":"read", "state":{"status":"running", "input":serde_json::to_string(&payload).unwrap()}},
            {"type":"tool", "id":"seven", "name":"read", "state":{"status":"running", "input":format!("{{partial {payload}")}},
            {"type":"tool", "id":"eight", "name":"read", "state":{"status":"completed", "content":[{"type":"text","text":payload}]}},
            {"type":"tool", "id":"nine", "name":"read", "state":{"status":"error", "content":[{"type":"text","text":42}],"error":{"message":"unused"}}},
            {"type":"tool", "id":"ten", "name":"read", "state":{"status":"completed", "content":[{"type":"text"}]}}
        ]);
        let data = json!({"content":content,"model":{"id":"model"},"tokens":{"input":1},"metadata":{"preserve":true}});
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        let (owned, spans) = both_demands(&path, |view, ctx| {
            Ok(v2::build("message", "assistant", view, &ParseOptions::lazy(), ctx)?.unwrap())
        });
        assert_eq!(
            serde_json::to_value(&owned).unwrap(),
            serde_json::to_value(&spans).unwrap()
        );
        let results = &owned[1].content;
        let text = |index: usize| match &results[index] {
            Block::ToolResult { content, .. } => content.inline_str().unwrap().to_string(),
            block => panic!("result {index}: {block:?}"),
        };
        assert_eq!(text(0), format!("{payload}\n\n{}", json!({"future":payload})));
        assert!(
            matches!(&results[1], Block::File { .. }),
            "file items follow their result"
        );
        assert_eq!((text(2), text(3)), (String::new(), "failure".into()));
        assert_eq!(
            (text(4), text(5), text(6)),
            (payload.clone(), String::new(), String::new())
        );
        let input = |index: usize| match &owned[0].content[index] {
            Block::ToolUse { input, .. } => input.resolve().unwrap().into_owned(),
            block => panic!("use {index}: {block:?}"),
        };
        assert_eq!((input(3), input(4), input(6)), (json!(42), json!(""), json!(true)));
        assert_eq!(input(7), json!(payload));
        assert_eq!(input(8), json!(format!("{{partial {payload}")));
        // A single long text result stays in the source, guarded.
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let messages = source
            .read_document(|view| {
                Ok(v2::build(
                    "message",
                    "assistant",
                    view,
                    &ParseOptions::lazy(),
                    record::Ctx::new(true, &source, &arena),
                )?
                .unwrap())
            })
            .unwrap();
        let Block::ToolResult { content, .. } = &messages[1].content[4] else {
            panic!("single text result")
        };
        assert!(matches!(content, crate::lazy::Text::Source(_)));
        std::fs::write(&path, b"[]").unwrap();
        assert!(content
            .try_resolve_prefix(&crate::lazy::Resolver::new(None), 8)
            .is_err());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn custom_database_filename_and_unknown_complete_records() -> Result<()> {
        let world = cv_testkit::World::new("opencode-custom-db");
        let path = world.base.join("custom-store.sqlite");
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,data TEXT); INSERT INTO session_v2 VALUES('ses_future','/fixture','future',1,2);")?;
        let raw = json!({"time":{"created":1},"nested":{"future":[1,2]},"payload":"opaque"});
        connection.execute(
            "INSERT INTO session_message VALUES('msg_future','ses_future','future-event',0,?1)",
            [raw.to_string()],
        )?;
        let adapter = OpenCode::with_db(path.clone());
        let reference = adapter.discover()?.remove(0);
        for spans in [false, true] {
            let full = crate::stream::collect_with(
                &adapter,
                &reference,
                &ParseOptions {
                    spans,
                    ..ParseOptions::full()
                },
            )?;
            assert!(full.messages.is_empty());
            assert_eq!(full.extra["opencode"]["unknown_message_types"]["future-event"], 1);
            let complete = crate::stream::collect_with(
                &adapter,
                &reference,
                &ParseOptions {
                    spans,
                    ..ParseOptions::complete()
                },
            )?;
            assert_eq!(complete.messages.len(), 1);
            let record = serde_json::to_value(&complete.messages[0])?["extra"]["_record"].clone();
            assert_eq!(record["nested"], raw["nested"]);
            assert_eq!(record["type"], "future-event");
            assert_eq!(complete.extra["opencode"]["unknown_message_types"]["future-event"], 1);
            let result = emit(
                &complete,
                &world.base.join(format!("export-{spans}")),
                &crate::emit::EmitOptions::default(),
            )?;
            let output = rusqlite::Connection::open(result.path)?;
            let (ty, data): (String, String) = output.query_row(
                "SELECT type,data FROM session_message WHERE session_id=?1",
                [result.new_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(ty, "future-event");
            assert_eq!(serde_json::from_str::<Value>(&data)?, raw);
        }
        Ok(())
    }

    #[test]
    fn file_history_timestamp_ties_use_stable_message_order() -> Result<()> {
        let world = cv_testkit::World::new("opencode-file-order");
        let root = world.base.join("storage");
        fs::create_dir_all(root.join("session/project"))?;
        fs::create_dir_all(root.join("message/ses_order"))?;
        fs::write(
            root.join("session/project/ses_order.json"),
            json!({"id":"ses_order","title":"order"}).to_string(),
        )?;
        for id in ["msg_z", "msg_a"] {
            fs::write(
                root.join(format!("message/ses_order/{id}.json")),
                json!({"id":id,"role":"user","time":{"created":1},"summary":{"body":id}}).to_string(),
            )?;
        }
        let adapter = OpenCode::with_root(root);
        let reference = adapter.discover()?.remove(0);
        for spans in [false, true] {
            let session = crate::stream::collect_with(
                &adapter,
                &reference,
                &ParseOptions {
                    spans,
                    ..ParseOptions::full()
                },
            )?;
            assert_eq!(
                session
                    .messages
                    .iter()
                    .map(|message| message.id.as_deref())
                    .collect::<Vec<_>>(),
                vec![Some("msg_a"), Some("msg_z")]
            );
        }
        Ok(())
    }

    #[test]
    fn system_record_is_the_system_prompt() {
        let mv: Value = serde_json::from_str(r#"{"id":"s","role":"system","time":{"created":1}}"#).unwrap();
        let out = build_messages(&mv, &parts(&[r#"{"type":"text","text":"You are opencode."}"#]));
        let mut m = out.into_iter().next().unwrap();
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );
        let mut s = Session::new("x", Harness::OpenCode);
        fold_into_session(&mut s, &mut m).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are opencode."));
        // the IR diet: a message model equal to the session's is dropped, a different one kept
        let mut a: Value =
            serde_json::from_str(r#"{"id":"a","role":"assistant","time":{"created":2},"modelID":"m1"}"#).unwrap();
        let mut first = build_messages(&a, &parts(&[r#"{"type":"text","text":"hi"}"#])).remove(0);
        fold_into_session(&mut s, &mut first).unwrap();
        assert_eq!(s.model.as_deref(), Some("m1"));
        assert_eq!(first.model, None);
        a["modelID"] = json!("m2");
        let mut second = build_messages(&a, &parts(&[r#"{"type":"text","text":"hi"}"#])).remove(0);
        fold_into_session(&mut s, &mut second).unwrap();
        assert_eq!(second.model.as_deref(), Some("m2"));
    }

    #[test]
    fn synthetic_text_still_rendered() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[r#"{"type":"text","text":"[search-mode] ...","synthetic":true}"#]);
        let out = build_messages(&mv, &p);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text.starts_with("[search-mode]")));
    }

    #[test]
    fn user_model_nested_under_model_object() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "model":{"providerID":"amazon-bedrock","modelID":"claude-opus"}}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"hi"}"#]);
        let out = build_messages(&mv, &p);
        assert_eq!(out[0].model.as_deref(), Some("claude-opus"));
    }

    /// End-to-end against on-disk fixtures for BOTH storage generations.
    #[test]
    fn parses_fixtures_both_generations() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");

        // Newer parts generation.
        let oc = OpenCode::with_root(base.join("parts_gen"));
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "one session in parts_gen fixture");
        let s = oc.parse(&refs[0]).unwrap();
        // Every OpenCode part fact is nested under `extra["opencode"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "bash")));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool && !m.content.is_empty()));
        assert_eq!(s.model.as_deref(), Some("anthropic.claude-sonnet-4-5"));

        // Older inline-summary generation (message records only, no part files).
        let oc2 = OpenCode::with_root(base.join("summary_gen"));
        let refs2 = oc2.discover().unwrap();
        assert_eq!(refs2.len(), 1, "one session in summary_gen fixture");
        let s2 = oc2.parse(&refs2[0]).unwrap();
        crate::harness::assert_no_flat_keys(&s2);
        assert!(s2
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text } if text.contains("debug comments"))));
    }

    #[test]
    fn tool_attachments_and_state_details_ride_with_the_result() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c1","tool":"read","state":{"status":"completed","input":{"filePath":"x.png"},"output":"","title":"x.png","metadata":{"preview":"…"},"time":{"start":1,"end":2},"attachments":[{"type":"file","mime":"image/png","url":"data:image/png;base64,iVBOR","filename":"x.png"},{"type":"file","mime":"text/plain","url":"file:///notes.txt","filename":"notes.txt"}]}}"#,
        ]);
        let out = build_messages(&mv, &p);
        assert_eq!(out.len(), 2);
        let tool = &out[1];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.content.len(), 3, "result + two attachments");
        match &tool.content[0] {
            Block::ToolResult { details, .. } => {
                let d = details.as_ref().expect("details");
                assert_eq!(d["title"], "x.png");
                assert_eq!(d["time"]["end"], 2);
                assert_eq!(d["metadata"]["preview"], "…");
            }
            b => panic!("expected tool result, got {b:?}"),
        }
        assert!(
            matches!(&tool.content[1], Block::Image { media_type, .. } if media_type.as_deref() == Some("image/png"))
        );
        assert!(matches!(&tool.content[2], Block::File { path, .. } if path.as_deref() == Some("notes.txt")));
    }

    #[test]
    fn error_name_is_lifted() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},
                "error":{"name":"ContentFilterError","data":{"message":"blocked"}}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        let m = &out[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::Assistant, MessageKind::Error, Origin::Harness)
        );
        assert_eq!(m.extra["opencode"]["error_name"], "ContentFilterError");
        assert_eq!(m.extra["opencode"]["error"]["data"]["message"], "blocked");
        // an abort that kept partial text is still an Error, text and all
        let aborted: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},"error":{"name":"MessageAbortedError","data":{}}}"#,
        )
        .unwrap();
        let out = build_messages(&aborted, &parts(&[r#"{"type":"text","text":"I was about to"}"#]));
        assert_eq!(out[0].kind, MessageKind::Error);
        assert_eq!(out[0].text().as_deref(), Some("I was about to"));
    }

    /// The real `opencode.db` DDL (opencode `fee476bb`, `packages/core/src/session/sql.ts`), minus
    /// the `project` foreign key. `with_metadata` adds the 2026-05 `session.metadata` column.
    #[cfg(feature = "sqlite")]
    fn make_db(path: &std::path::Path, with_metadata: bool) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        let metadata_col = if with_metadata { ", `metadata` text" } else { "" };
        conn.execute_batch(&format!(
            "CREATE TABLE `session` (`id` text PRIMARY KEY, `project_id` text NOT NULL, `parent_id` text, \
             `slug` text NOT NULL, `directory` text NOT NULL, `title` text NOT NULL, `version` text NOT NULL, \
             `share_url` text, `summary_additions` integer, `summary_deletions` integer, `summary_files` integer, \
             `summary_diffs` text, `revert` text, `permission` text, `time_created` integer NOT NULL, \
             `time_updated` integer NOT NULL, `time_compacting` integer, `time_archived` integer, \
             `workspace_id` text, `path` text, `agent` text, `model` text, `cost` real DEFAULT 0 NOT NULL, \
             `tokens_input` integer DEFAULT 0 NOT NULL, `tokens_output` integer DEFAULT 0 NOT NULL, \
             `tokens_reasoning` integer DEFAULT 0 NOT NULL, `tokens_cache_read` integer DEFAULT 0 NOT NULL, \
             `tokens_cache_write` integer DEFAULT 0 NOT NULL{metadata_col});\
             CREATE TABLE `message` (`id` text PRIMARY KEY, `session_id` text NOT NULL, \
             `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);\
             CREATE TABLE `part` (`id` text PRIMARY KEY, `message_id` text NOT NULL, `session_id` text NOT NULL, \
             `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);"
        ))
        .unwrap();
        conn
    }

    #[cfg(feature = "sqlite")]
    fn seed_session(conn: &rusqlite::Connection, sid: &str, with_metadata: bool) {
        let extra_cols = if with_metadata { ", metadata" } else { "" };
        let extra_vals = if with_metadata { ", '{\"origin\":\"test\"}'" } else { "" };
        conn.execute(
            &format!(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, \
                 time_updated, agent, model, cost, tokens_input, tokens_output, tokens_cache_read{extra_cols}) VALUES \
                 (?1, 'proj', 'ses_PARENT', '', '/home/dev/proj', 'Removing debug comments', '1.15.10', \
                 1767591058589, 1767591138434, 'Sisyphus', '{{\"id\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\",\"variant\":\"default\"}}', \
                 0.27, 42987, 2055, 123762{extra_vals})"
            ),
            rusqlite::params![sid],
        )
        .unwrap();
        // Message rows: `data` = Info minus id/sessionID (exactly what opencode writes).
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1767591060000, 1767591060000, ?3)",
            rusqlite::params![
                "msg_user01",
                sid,
                r#"{"role":"user","time":{"created":1767591060000},"model":{"providerID":"anthropic","modelID":"claude-sonnet-4-5"},"agent":"Sisyphus"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1767591135919, 1767591138413, ?3)",
            rusqlite::params![
                "msg_asst01",
                sid,
                r#"{"role":"assistant","time":{"created":1767591135919,"completed":1767591138413},"parentID":"msg_user01","modelID":"claude-sonnet-4-5","providerID":"anthropic","agent":"Sisyphus","path":{"cwd":"/home/dev/proj","root":"/home/dev/proj"},"cost":0.03,"tokens":{"input":8708,"output":7,"reasoning":0,"cache":{"read":20627,"write":0}},"finish":"tool-calls"}"#
            ],
        )
        .unwrap();
        // Part rows: `data` = Part minus id/sessionID/messageID; ordered by id.
        let add = |pid: &str, mid: &str, data: &str| {
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 1, 1, ?4)",
                rusqlite::params![pid, mid, sid, data],
            )
            .unwrap();
        };
        add(
            "prt_a",
            "msg_user01",
            r#"{"type":"text","text":"remove the debug comments"}"#,
        );
        add("prt_b", "msg_asst01", r#"{"type":"step-start","snapshot":"c2e2"}"#);
        add(
            "prt_c",
            "msg_asst01",
            r#"{"type":"reasoning","text":"scanning…","metadata":{"anthropic":{"signature":"SIG"}}}"#,
        );
        add("prt_d", "msg_asst01", r#"{"type":"text","text":"Reverted."}"#);
        add(
            "prt_e",
            "msg_asst01",
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"ls","metadata":{"exit":0},"time":{"start":1,"end":2}}}"#,
        );
    }

    /// Reading a running OpenCode's store writes nothing there: discovery plus a full stream of
    /// every session leave each file's inode, size, mtime and bytes (`-shm` included) unchanged,
    /// both with WAL frames pending and after a checkpoint, and the WAL-only rows are read.
    #[cfg(feature = "sqlite")]
    #[test]
    fn reading_a_live_wal_store_writes_nothing_in_it() {
        use crate::harness::store::sqlite::{live_store, store_state};
        let dir = std::env::temp_dir().join(format!("cv-opencode-live-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("opencode.db");
        drop(make_db(&db, true));
        let writer = live_store(&db, "SELECT 1");
        seed_session(&writer, "ses_live", true);
        assert!(fs::metadata(dir.join("opencode.db-wal")).unwrap().len() > 0);
        for checkpointed in [false, true] {
            if checkpointed {
                writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            }
            let before = store_state(&dir);
            let adapter = OpenCode::with_db(db.clone());
            let refs = adapter.discover().unwrap();
            assert_eq!(refs.len(), 1);
            for reference in &refs {
                let mut session = crate::stream::collect_with(&adapter, reference, &ParseOptions::lazy()).unwrap();
                session.materialize().unwrap();
                assert!(session
                    .messages
                    .iter()
                    .any(|m| m.text().as_deref() == Some("Reverted.")));
            }
            assert_eq!(store_state(&dir), before, "checkpointed: {checkpointed}");
        }
        drop(writer);
        fs::remove_dir_all(dir).ok();
    }

    /// A `session_message` cell that is not TEXT, empty, not UTF-8 or not exactly one JSON value
    /// is a skipped line in every demand, never an error and never a message.
    #[cfg(feature = "sqlite")]
    #[test]
    fn malformed_v2_cells_are_skipped_lines() {
        let world = cv_testkit::World::new("opencode-cell");
        let db = world.base.join("opencode.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,data); INSERT INTO session_v2 VALUES('session','/tmp','test',1,1); INSERT INTO session_message VALUES('message','session','assistant',1,'{broken');").unwrap();
        let adapter = OpenCode::with_db(db);
        let reference = adapter.discover().unwrap().remove(0);
        for ty in ["assistant", "shell"] {
            connection.execute("UPDATE session_message SET type=?1", [ty]).unwrap();
            for expression in [
                "'{broken'",
                "''",
                "CAST(X'FF' AS TEXT)",
                "X'7B7D'",
                "NULL",
                "'{\"output\":\"\\ud800\"}'",
                "'{\"output\":1e400}'",
                "'{\"output\":\"\\ud800\",\"output\":\"valid duplicate winner\"}'",
            ] {
                connection
                    .execute_batch(&format!("UPDATE session_message SET data = {expression}"))
                    .unwrap();
                for opts in [ParseOptions::bulk(), ParseOptions::lazy()] {
                    let session = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
                    assert!(session.messages.is_empty(), "{expression}");
                    assert_eq!(session.extra["cv"]["skipped_lines"], 1, "{expression}");
                }
            }
        }
    }

    #[cfg(feature = "sqlite")]
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_v1_discovery_counts_follow_readable_final_roles() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-role-count-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("opencode.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,data TEXT);").unwrap();
        let cases = [
            (
                r#"{"role":"system","role":"assistant","summary":{"body":"body"}}"#,
                0,
                1,
            ),
            (
                r#"{"role":"assistant","role":"system","summary":{"body":"body"}}"#,
                1,
                0,
            ),
            (r#"{role:'user',summary:{body:'body'}}"#, 1, 0),
            (
                r#"{"role":"assistant","summary":{"body":"body"},"ignored":1e400}"#,
                1,
                0,
            ),
            (
                r#"{"role":"assistant","summary":{"body":"body"},"ignored":"\ud800"}"#,
                1,
                0,
            ),
            (r#"{"role":"\u0075ser","summary":{"body":"body"}}"#, 1, 1),
        ];
        for (index, (data, old, new)) in cases.iter().enumerate() {
            let id = format!("s{index}");
            connection
                .execute("INSERT INTO session VALUES(?1,'/perf','roles',1,1)", [&id])
                .unwrap();
            connection
                .execute("INSERT INTO message VALUES(?1,?1,1,?2)", rusqlite::params![id, data])
                .unwrap();
            let old_count: usize = connection.query_row("SELECT COUNT(*) FROM message WHERE session_id=?1 AND json_extract(data,'$.role') IN ('user','assistant')", [&id], |row| row.get(0)).unwrap();
            assert_eq!(old_count, *old);
            let adapter = OpenCode::with_db(path.clone());
            let reference = adapter
                .discover()
                .unwrap()
                .into_iter()
                .find(|reference| reference.id == id)
                .unwrap();
            assert_eq!(reference.message_count, *new);
            let session = adapter.parse(&reference).unwrap();
            assert_eq!(
                session
                    .messages
                    .iter()
                    .filter(|message| matches!(message.role, Role::User | Role::Assistant))
                    .count(),
                *new
            );
        }
        drop(connection);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn cold_wal_selected_text_keeps_original_guard_without_creating_sidecars() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-cold-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("opencode.db");
        let writer = make_db(&db, false);
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        seed_session(&writer, "session", false);
        let body = "selected ".repeat(8192);
        writer
            .execute(
                "UPDATE part SET data=?1 WHERE id='prt_a'",
                [json!({"type":"text","text":body}).to_string()],
            )
            .unwrap();
        let shm = dir.join("opencode.db-shm");
        fs::remove_file(&shm).unwrap();
        let adapter = OpenCode::with_db(db);
        let reference = adapter.discover().unwrap().remove(0);
        assert_eq!(reference.message_count, 2);
        let mut messages = Vec::new();
        let mut sink = crate::stream::TryFnSink::new(|message| {
            messages.push(message);
            Ok(crate::stream::Flow::Continue)
        });
        adapter.stream(&reference, &ParseOptions::lazy(), &mut sink).unwrap();
        let Block::Text { text } = &messages[0].content[0] else {
            panic!("selected text")
        };
        assert_eq!(
            text.try_resolve_prefix(&crate::lazy::Resolver::new(None), u64::MAX)
                .unwrap()
                .as_ref(),
            body
        );
        assert!(!shm.exists(), "original store must remain untouched");
        writer
            .execute("UPDATE session SET title='changed' WHERE id='session'", [])
            .unwrap();
        assert!(
            text.try_resolve_prefix(&crate::lazy::Resolver::new(None), 8).is_err(),
            "copied payload must validate original WAL"
        );
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_snapshot_rejects_wal_mutation_even_after_sink_failure() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-wal-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("opencode.db");
        let writer = make_db(&db, false);
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        seed_session(&writer, "session", false);
        let adapter = OpenCode::with_db(db);
        let reference = adapter.discover().unwrap().remove(0);
        let mut sink = crate::stream::TryFnSink::new(|_message| {
            writer
                .execute("UPDATE session SET title = 'changed' WHERE id = 'session'", [])
                .unwrap();
            anyhow::bail!("consumer cap")
        });
        let error = adapter
            .stream(&reference, &ParseOptions::lazy(), &mut sink)
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed"), "{error:#}");
        assert!(!format!("{error:#}").contains("consumer cap"));
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn db_store_discover_and_parse() {
        for with_metadata in [false, true] {
            let dir = std::env::temp_dir().join(format!("cv-opencode-db-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            let db = dir.join("opencode.db");
            let conn = make_db(&db, with_metadata);
            seed_session(&conn, "ses_DB01", with_metadata);
            drop(conn);

            let oc = OpenCode::with_db(db.clone());
            let refs = oc.discover().unwrap();
            assert_eq!(refs.len(), 1);
            let r = &refs[0];
            assert_eq!(r.id, "ses_DB01");
            assert_eq!(r.path, db, "a db-backed ref points at the database");
            assert_eq!(r.cwd.as_deref(), Some(std::path::Path::new("/home/dev/proj")));
            assert_eq!(r.title.as_deref(), Some("Removing debug comments"));
            assert_eq!(
                r.message_count, 2,
                "user + assistant turns, from the role inside `data`"
            );
            assert!(r.created_at.is_some() && r.updated_at >= r.created_at);

            let s = oc.parse(r).unwrap();
            crate::harness::assert_no_flat_keys(&s);
            assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-5"), "session.model.id wins");
            assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/proj")));
            assert_eq!(s.lineage.parent.as_deref(), Some("ses_PARENT"), "parent_id is lineage");
            let bag = &s.extra["opencode"];
            assert_eq!(bag["agent"], "Sisyphus");
            assert!(bag.get("parent_id").is_none(), "first-class facts leave the bag");
            assert_eq!(bag["tokens_input"], 42987);
            assert_eq!(bag["model"]["providerID"], "anthropic");
            assert!(bag.get("slug").is_none(), "blank columns stay out of extra");
            assert_eq!(
                bag.get("metadata").map(|m| m["origin"] == "test"),
                with_metadata.then_some(true)
            );
            assert_eq!(
                s.extra.len(),
                1,
                "session facts live only in the harness bag: {:?}",
                s.extra
            );
            // user, assistant, tool — the same mapping the JSON tree gets
            let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
            assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool]);
            let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
            assert_eq!(
                kinds,
                vec![MessageKind::Prompt, MessageKind::Reply, MessageKind::ToolResult]
            );
            // both records name the session's own model, so neither carries it (IR diet)
            assert!(s.messages.iter().all(|m| m.model.is_none()), "{:?}", s.messages);
            assert_eq!(s.messages[1].usage.as_ref().unwrap().cost_usd, Some(0.03));
            for m in &s.messages {
                assert!(m.extra.keys().all(|k| k == "opencode"), "{:?}", m.extra);
            }
            assert_eq!(
                s.messages[0].id.as_deref(),
                Some("msg_user01"),
                "row id hydrated into the record"
            );
            assert_eq!(s.messages[1].parent_id.as_deref(), Some("msg_user01"));
            assert!(
                matches!(&s.messages[1].content[0], Block::Thinking { signature, .. } if signature.as_deref() == Some("SIG"))
            );
            assert!(matches!(&s.messages[1].content[1], Block::Text { text } if text == "Reverted."));
            assert!(matches!(&s.messages[2].content[0], Block::ToolResult { content, .. } if content == "a\nb"));
            assert_eq!(s.messages[1].usage.as_ref().unwrap().cache_read_tokens, Some(20627));
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn db_wins_over_a_json_duplicate_and_json_still_fills_gaps() {
        // The JSON fixture holds ses_FIX01; a database holding the same id (the import left both)
        // must yield ONE ref, the db-backed one — while a JSON-only session is still discovered.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/parts_gen");
        let dir = std::env::temp_dir().join(format!("cv-opencode-dedupe-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let conn = make_db(&db, false);
        seed_session(&conn, "ses_FIX01", false);
        drop(conn);
        let oc = OpenCode {
            root: Some(base),
            db: Some(db.clone()),
        };
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "duplicate collapsed: {refs:?}");
        assert_eq!(refs[0].path, db);
        // and with a db that lacks it, the JSON copy is used
        let oc_json_only = OpenCode {
            root: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/parts_gen")),
            db: None,
        };
        assert_eq!(oc_json_only.discover().unwrap().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// A `reasoning` part whose text is empty but which carries a provider signature is the WHOLE
    /// of a redacted thinking turn. Dropping it used to delete the turn's only block, and an
    /// assistant message with no content is discarded — which silently cost a real ported session
    /// 134 assistant turns along with their usage, timestamps and ids.
    #[test]
    fn signature_only_reasoning_keeps_its_turn() {
        let mv = serde_json::json!({"id": "msg_1", "role": "assistant", "modelID": "claude-x"});
        let parts = vec![serde_json::json!({
            "type": "reasoning",
            "text": "",
            "metadata": {"anthropic": {"signature": "SIGBLOB=="}}
        })];
        let msgs = build_messages(&mv, &parts);
        assert_eq!(msgs.len(), 1, "the turn must survive: {msgs:?}");
        assert!(
            matches!(
                msgs[0].content.as_slice(),
                [Block::Thinking { signature: Some(sig), redacted: false, .. }] if sig == "SIGBLOB=="
            ),
            "signature-only thinking preserved: {:?}",
            msgs[0].content
        );

        // A reasoning part with neither text nor signature carries nothing and is still dropped.
        let empty = vec![serde_json::json!({"type": "reasoning", "text": ""})];
        assert!(
            build_messages(&mv, &empty).is_empty(),
            "a contentless part adds no turn"
        );
    }
}
