//! A persistent SQLite catalog of discovered sessions: `id → (harness, path, metadata)` — and,
//! since v0.9.12, the **hot read path** for listing the fleet.
//!
//! [`crate::find`] used to resolve one session id by calling `discover()` on **every** adapter and
//! scanning the whole fleet — O(all sessions across all harnesses) for a single `cv show <id>`. The
//! catalog turns that into an indexed lookup: [`crate::discover_all`] publishes every successful harness's rows
//! here, and `find` consults it first (O(1)/O(log n)), falling back to a full scan only on a miss
//! (which then re-warms the catalog).
//!
//! The same rows now also serve [`crate::sessions`], which answers `cv ls`/`stats`
//! without touching the fleet at all. Correctness rests on the **freshness probe**
//! ([`Reader::probe_stale`]): every full discovery records, per harness, the directories that directly
//! contain session files (plus all ancestors up to the harness root) and — for the SQLite-backed
//! harnesses — the database files and their WAL sidecars, each with its mtime at scan time (the `watched`
//! table). The probe re-stats those few hundred paths in milliseconds; any new/changed/vanished
//! entry marks its harness for re-discovery.
//!
//! Sibling tables: `scan_cache` is [`crate::discover_cache`]'s per-file `(mtime, size) → refs`
//! store (it owns the reads/writes; the DDL lives here so every opener sees one schema), and
//! `catalog_meta` holds the `last_full_sync` stamp behind the bounded-staleness backstop.
//!
//! Best-effort throughout: any sqlite error is swallowed and the caller falls back to scanning, so
//! correctness never depends on the catalog (a stale row is filtered by `path.exists()` at lookup).
//! Behind the `sqlite` feature (default-on); no-op fallbacks keep the scan path working without it.

use crate::ir::{Harness, SessionRef};

mod reader;
pub(crate) use reader::Reader;
mod writer;
#[cfg(feature = "sqlite")]
use std::path::{Path, PathBuf};
pub use writer::Writer;

pub(crate) type PrimaryReads = std::collections::HashMap<std::path::PathBuf, crate::source::ReadSet>;

pub(crate) type Publication<'a> = (
    Harness,
    &'a [SessionRef],
    Vec<(std::path::PathBuf, i64)>,
    &'a PrimaryReads,
);

/// SQL row loading may precede parallel discovery; source identity capture must not.
pub(crate) fn capture_primary_reads_from(a: &dyn crate::Adapter, refs: &[SessionRef]) -> PrimaryReads {
    let mut out = PrimaryReads::default();
    for r in refs {
        let path = a.resume_source(r);
        out.entry(path).or_insert_with_key(|path| {
            let mut reads = crate::source::ReadSet::default();
            reads.record(path);
            reads
        });
    }
    out
}

#[cfg(feature = "sqlite")]
mod imp {
    use super::*;
    use rusqlite::{params, Connection, Row};
    use std::collections::{BTreeSet, HashSet};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// A directory (or db file) whose recorded mtime is younger than this at record time is stamped
    /// `mtime_ns = 0` ("always stale"): a session file may have landed *between* discovery
    /// enumerating the directory and us stat'ing it, so the next probe re-discovers that harness
    /// once and re-records a trustworthy mtime. Self-quiescing: an actively-written harness pays
    /// one cheap re-discovery per probe until it goes idle for this long.
    const WATCH_FUDGE: Duration = Duration::from_secs(2);

    /// An optional WAL was absent at discovery. Unlike `0`, stable absence is fresh; its
    /// appearance must invalidate discovery even if the database itself has not changed.
    const WATCH_ABSENT: i64 = -1;

    pub(crate) fn db_path() -> Option<PathBuf> {
        // Back-compat: also honor the legacy `$CLAURDVOYANT_HOME` / `claurdvoyant` cache dir so a
        // catalog built before the rename keeps being used.
        let env = crate::env::Env::current();
        let dir = env
            .var_os("CLUSTERVISION_HOME")
            .or_else(|| env.var_os("CLAURDVOYANT_HOME"))
            .map(PathBuf::from)
            .or_else(|| {
                let cache = env.cache_dir()?;

                let new = cache.join("clustervision");
                let legacy = cache.join("claurdvoyant");
                Some(if !new.exists() && legacy.exists() { legacy } else { new })
            })?;
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.join("catalog.db"))
    }

    /// An open connection to the shared catalog db (WAL + busy timeout, `sessions` DDL applied),
    /// for sibling catalog tables — the event catalog ([`crate::events`]) adds its own DDL on top.
    /// `None` on any error; callers degrade.
    pub(crate) fn open_db() -> Option<Connection> {
        open()
    }

    #[cfg(test)]
    thread_local! { static OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

    fn open() -> Option<Connection> {
        open_at(&db_path()?)
    }

    pub(crate) fn open_at(path: &Path) -> Option<Connection> {
        #[cfg(test)]
        OPEN_COUNT.with(|n| n.set(n.get() + 1));
        let mut conn = Connection::open(path).ok()?;
        // WAL + a busy timeout so concurrent cv processes don't error each other out.
        conn.busy_timeout(Duration::from_secs(5)).ok()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions(
                 harness       TEXT NOT NULL,
                 id            TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 cwd           TEXT,
                 title         TEXT,
                 created_at    INTEGER,
                 updated_at    INTEGER,
                 message_count INTEGER,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(id);
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
             CREATE TABLE IF NOT EXISTS watched(
                 harness  TEXT NOT NULL,
                 path     TEXT NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 PRIMARY KEY(harness, path)
             );
             CREATE TABLE IF NOT EXISTS primary_sources(
                 harness TEXT NOT NULL, path TEXT NOT NULL, reads TEXT NOT NULL,
                 PRIMARY KEY(harness,path)
             );
             CREATE TABLE IF NOT EXISTS scan_cache(
                 path     TEXT PRIMARY KEY,
                 mtime_ns INTEGER NOT NULL,
                 size     INTEGER NOT NULL,
                 refs     TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS affordance_counts(
                 harness TEXT NOT NULL,
                 id      TEXT NOT NULL,
                 path    TEXT NOT NULL,
                 sig_a   INTEGER NOT NULL,
                 sig_b   INTEGER NOT NULL,
                 counts  TEXT NOT NULL,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE TABLE IF NOT EXISTS catalog_meta(
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- The seek store (0.14 and earlier) is gone; see `crate::offsets`.
             DROP TABLE IF EXISTS msg_offsets;
             DROP TABLE IF EXISTS msg_offsets_schema;",
        )
        .ok()?;
        // `sessions` predates the catalog read path, which needs sub-second timestamps to
        // reproduce discovery's ordering exactly. The original second-precision columns stay (the
        // event catalog joins them); these ride alongside. Errors ignored: a concurrent open may
        // have just added them (and a pre-migration reader simply sees NULL → second fallback).
        let has_ns: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name='updated_ns')",
                [],
                |r| r.get(0),
            )
            .ok()?;
        if !has_ns {
            let _ = conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN created_ns INTEGER;
                 ALTER TABLE sessions ADD COLUMN updated_ns INTEGER;",
            );
        }
        let has_version: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('scan_cache') WHERE name='interpretation_version')",
                [],
                |row| row.get(0),
            )
            .ok()?;
        if !has_version {
            // Nullable: an older binary's INSERT OR REPLACE omits it and produces a stale row.
            let _ = conn.execute_batch("ALTER TABLE scan_cache ADD COLUMN interpretation_version INTEGER;");
        }
        let _ = conn.execute_batch("ALTER TABLE scan_cache ADD COLUMN source_reads TEXT;");
        let _ = conn.execute_batch("ALTER TABLE scan_cache ADD COLUMN continuation TEXT;");
        let version = crate::discover_cache::INTERPRETATION_VERSION.to_string();
        let old: Option<String> = conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                [],
                |row| row.get(0),
            )
            .ok();
        if old.as_deref() != Some(version.as_str()) {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .ok()?;
            // Recheck under the writer lock: a concurrent new process may already have upgraded.
            let old: Option<String> = tx
                .query_row(
                    "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                    [],
                    |row| row.get(0),
                )
                .ok();
            if old.as_deref() != Some(version.as_str()) {
                tx.execute("DELETE FROM watched", []).ok()?;
                tx.execute("DELETE FROM primary_sources", []).ok()?;
                tx.execute("DELETE FROM catalog_meta WHERE key='last_full_sync'", [])
                    .ok()?;
                tx.execute(
                    "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('discovery_version',?1)",
                    [&version],
                )
                .ok()?;
                if old.is_some() {
                    // An earlier cv built this catalog: the next full discovery says why it is slow.
                    tx.execute(
                        "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('rediscovery_notice',?1)",
                        [&version],
                    )
                    .ok()?;
                }
                tx.commit().ok()?;
                crate::discover_cache::invalidate_interpretation(path);
            }
        }
        Some(conn)
    }

    fn watch_is_fresh(path: &Path, recorded: i64) -> bool {
        match std::fs::metadata(path) {
            Ok(md) => {
                recorded != 0
                    && recorded != WATCH_ABSENT
                    && md.modified().ok().is_some_and(|t| {
                        t.duration_since(UNIX_EPOCH)
                            .ok()
                            .is_some_and(|d| d.as_nanos() as i64 == recorded)
                    })
            }
            Err(e) => recorded == WATCH_ABSENT && e.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// The SELECT column list [`row_to_ref`] decodes — keep the two in lockstep.
    const REF_COLUMNS: &str = "harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns";

    /// A timestamp from the nanosecond column at `ns_idx`, falling back to the second-precision
    /// column at `s_idx` (rows written before the `_ns` migration, or dates outside the i64-ns
    /// range — ±~292 years of 1970, where `timestamp_nanos_opt` stores NULL).
    fn ts_col(row: &Row, ns_idx: usize, s_idx: usize) -> rusqlite::Result<Option<chrono::DateTime<chrono::Utc>>> {
        if let Some(ns) = row.get::<_, Option<i64>>(ns_idx)? {
            return Ok(Some(chrono::DateTime::from_timestamp_nanos(ns)));
        }
        Ok(row
            .get::<_, Option<i64>>(s_idx)?
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)))
    }

    /// One row (in [`REF_COLUMNS`] order) → one `SessionRef`. `Ok(None)` for rows whose harness
    /// this binary doesn't know (a newer binary's rows — skipped, never deleted by us).
    fn row_to_ref(row: &Row) -> rusqlite::Result<Option<SessionRef>> {
        row_to_ref_projected(row, [false, false])
    }

    // Preserve cell type/UTF8 acceptance even when a hit overrides the catalog hint. Borrow the
    // native SQLite cell for validation; do not allocate a String that output will discard.
    fn optional_hint(row: &Row, index: usize, overridden: bool) -> rusqlite::Result<Option<String>> {
        let value = row.get_ref(index)?;
        if matches!(value, rusqlite::types::ValueRef::Null) {
            return Ok(None);
        }
        let text = value
            .as_str()
            .map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error)))?;
        Ok((!overridden).then(|| text.to_owned()))
    }

    fn row_to_ref_projected(row: &Row, overrides: [bool; 2]) -> rusqlite::Result<Option<SessionRef>> {
        let harness: String = row.get(0)?;
        let Some(harness) = Harness::parse(&harness) else {
            return Ok(None);
        };
        Ok(Some(SessionRef {
            harness,
            id: row.get(1)?,
            path: PathBuf::from(row.get::<_, String>(2)?),
            cwd: optional_hint(row, 3, overrides[0])?.map(PathBuf::from),
            title: optional_hint(row, 4, overrides[1])?,
            created_at: ts_col(row, 8, 5)?,
            updated_at: ts_col(row, 9, 6)?,
            message_count: row.get::<_, i64>(7)? as usize,
        }))
    }

    /// Insert (or replace) every ref through `stmt` — shared by [`sync`] and [`Reader::replace_harness`].
    fn insert_refs(tx: &rusqlite::Transaction, refs: &[SessionRef]) -> rusqlite::Result<()> {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO sessions
             (harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        )?;
        for r in refs {
            stmt.execute(params![
                r.harness.as_str(),
                r.id,
                r.path.to_string_lossy(),
                r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                r.title,
                r.created_at.map(|t| t.timestamp()),
                r.updated_at.map(|t| t.timestamp()),
                r.message_count as i64,
                r.created_at.and_then(|t| t.timestamp_nanos_opt()),
                r.updated_at.and_then(|t| t.timestamp_nanos_opt()),
            ])?;
        }
        Ok(())
    }

    /// Upsert every ref in one transaction, deleting nothing — the *additive* sync for callers
    /// that know about one session (event ingest, tests). Full discovery uses [`Reader::replace_harness`]
    /// instead so vanished sessions actually disappear.
    pub fn sync(refs: &[SessionRef]) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        if insert_refs(&tx, refs).is_ok() {
            let _ = tx.commit();
        }
    }

    /// Bracket known primary sources before discovery. New uncached paths stay unknown until
    /// a later scoped pass; never bless an identity sampled after its metadata was read.
    pub(super) fn primary_refs_in(conn: &Connection, h: Option<Harness>) -> Vec<SessionRef> {
        // Source selection uses only harness, ID and path. Do not decode title/cwd/time metadata
        // that discovery will replace; this inventory exists solely to bracket primary reads.
        let sql = if h.is_some() {
            "SELECT harness,id,path FROM sessions WHERE harness=?1 ORDER BY rowid"
        } else {
            "SELECT harness,id,path FROM sessions WHERE ?1 IS NULL ORDER BY rowid"
        };
        let Ok(mut stmt) = conn.prepare(sql) else {
            return Default::default();
        };
        let Ok(rows) = stmt.query_map([h.map(Harness::as_str)], |row| {
            let name: String = row.get(0)?;
            let Some(harness) = Harness::parse(&name) else {
                return Ok(None);
            };
            Ok(Some(SessionRef {
                harness,
                id: row.get(1)?,
                path: PathBuf::from(row.get::<_, String>(2)?),
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: 0,
            }))
        }) else {
            return Default::default();
        };
        rows.filter_map(Result::transpose)
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_or_default()
    }

    /// Replace one harness's catalog rows *and* its watch entries in a single transaction — the
    /// post-discovery sync. Deleting first is what makes vanished sessions vanish from
    /// [`all_sessions`]; the transaction keeps concurrent readers from seeing the gap.
    #[cfg(test)]
    pub(crate) fn replace_harness(h: Harness, refs: &[SessionRef], watches: &[(PathBuf, i64)], before: &PrimaryReads) {
        if let Some(mut conn) = open() {
            replace_harness_in(&mut conn, h, refs, watches, before);
        }
    }

    #[cfg(test)]
    pub(super) fn replace_harness_in(
        conn: &mut Connection,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, i64)],
        before: &PrimaryReads,
    ) {
        let sources = prepare_sources(h, refs, before);
        replace_prepared_in(conn, h, refs, watches, &sources);
    }

    pub(super) fn replace_prepared_in(
        conn: &mut Connection,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, i64)],
        sources: &SourceRows,
    ) {
        let Ok(tx) = conn.transaction() else { return };
        if publish_harness_in(&tx, h, refs, watches, sources, true).is_ok() {
            let _ = tx.commit();
        }
    }

    fn matches_sessions_in<'a>(
        conn: &Connection,
        refs: impl Iterator<Item = &'a SessionRef>,
    ) -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare(&format!("SELECT {REF_COLUMNS} FROM sessions ORDER BY rowid"))?;
        let rows = stmt.query_map([], row_to_ref)?;
        let mut refs = refs;
        for row in rows {
            let Some(a) = row? else { return Ok(false) };
            let Some(b) = refs.next() else { return Ok(false) };
            if a.harness != b.harness
                || a.id != b.id
                || a.path != b.path
                || a.cwd != b.cwd
                || a.title != b.title
                || a.created_at != b.created_at
                || a.updated_at != b.updated_at
                || a.message_count != b.message_count
            {
                return Ok(false);
            }
        }
        Ok(refs.next().is_none())
    }

    #[cfg(test)]
    fn matches_sessions<'a>(refs: impl Iterator<Item = &'a SessionRef>) -> bool {
        open()
            .and_then(|conn| matches_sessions_in(&conn, refs).ok())
            .unwrap_or(false)
    }

    /// Compare and publish the full ordered output in one SQL snapshot. Cache guards are
    /// prepared before taking the writer lock, preserving the cache-to-SQL lock order.
    #[cfg(test)]
    fn publish_all(rows: &[Publication<'_>]) {
        if let Some(mut reader) = Reader::open() {
            reader.publish_all(rows);
        }
    }

    pub(super) fn publish_all_in(conn: &mut Connection, rows: &[Publication<'_>], sources: &[SourceRows]) {
        let Ok(tx) = conn.transaction() else { return };
        let Ok(equal) = matches_sessions_in(&tx, rows.iter().flat_map(|(_, refs, _, _)| refs.iter())) else {
            return;
        };
        for ((h, refs, watches, _), sources) in rows.iter().zip(sources) {
            if publish_harness_in(&tx, *h, refs, watches, sources, !equal).is_err() {
                return;
            }
        }
        if stamp_full_sync_in(&tx).is_ok() {
            let _ = tx.commit();
        }
    }

    /// Reconcile existing watch/guard rows without rewriting equal values.
    fn reconcile_rows(
        tx: &rusqlite::Transaction,
        h: Harness,
        table: &str,
        value: &str,
        rows: &std::collections::HashMap<String, rusqlite::types::Value>,
    ) -> rusqlite::Result<()> {
        let mut stmt = tx.prepare(&format!("SELECT path,{value} FROM {table} WHERE harness=?1"))?;
        let old: std::collections::HashMap<String, rusqlite::types::Value> = stmt
            .query_map([h.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for path in old.keys().filter(|path| !rows.contains_key(*path)) {
            tx.execute(
                &format!("DELETE FROM {table} WHERE harness=?1 AND path=?2"),
                params![h.as_str(), path],
            )?;
        }
        let mut insert = tx.prepare(&format!(
            "INSERT OR REPLACE INTO {table}(harness,path,{value}) VALUES(?1,?2,?3)"
        ))?;
        for (path, val) in rows.iter().filter(|(path, val)| old.get(*path) != Some(*val)) {
            insert.execute(params![h.as_str(), path, val])?;
        }
        Ok(())
    }

    pub(super) type SourceRows = std::collections::HashMap<String, rusqlite::types::Value>;

    pub(super) fn prepare_sources(h: Harness, refs: &[SessionRef], before: &PrimaryReads) -> SourceRows {
        // Resolve cache guards before acquiring the SQLite writer: cache persistence takes
        // the cache lock first, so reversing that order here could deadlock.
        let mut source_rows = std::collections::HashMap::new();
        let adapter = crate::harness::for_harness(h);
        let mut seen = HashSet::new();
        for r in refs {
            if !seen.insert(&r.path) {
                continue;
            }
            let primary = adapter.as_ref().map_or_else(|| r.path.clone(), |a| a.resume_source(r));
            let reads = before
                .get(&primary)
                .cloned()
                .or_else(|| crate::discover_cache::captured_reads(&primary));
            if let Some(reads) = reads {
                if let Ok(json) = serde_json::to_string(&reads) {
                    source_rows.insert(
                        r.path.to_string_lossy().into_owned(),
                        rusqlite::types::Value::Text(json),
                    );
                }
            }
        }
        source_rows
    }

    fn publish_harness_in(
        tx: &rusqlite::Transaction,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, i64)],
        source_rows: &SourceRows,
        rewrite_sessions: bool,
    ) -> rusqlite::Result<()> {
        if rewrite_sessions {
            tx.execute("DELETE FROM sessions WHERE harness=?1", params![h.as_str()])?;
            insert_refs(tx, refs)?;
        }
        let watch_rows = watches
            .iter()
            .map(|(path, mtime)| {
                (
                    path.to_string_lossy().into_owned(),
                    rusqlite::types::Value::Integer(*mtime),
                )
            })
            .collect();
        reconcile_rows(tx, h, "primary_sources", "reads", source_rows)?;
        reconcile_rows(tx, h, "watched", "mtime_ns", &watch_rows)
    }

    /// Compute the watch set for one harness from a finished discovery: the harness roots, every
    /// ancestor directory between each session file and its root (so a new project/day directory
    /// anywhere in the tree bumps a watched mtime), and — for the SQLite-backed harnesses — each
    /// session's backing file and its WAL (in-place writes change neither directory mtimes nor
    /// necessarily the main database). Optional CLI metadata and absent WALs remain watched. Other paths that can't be
    /// stat'd record `0`, as do recently-modified paths (see [`WATCH_FUDGE`]).
    pub(crate) fn watches_for(h: Harness, roots: &[PathBuf], refs: &[SessionRef]) -> Vec<(PathBuf, i64)> {
        let mut paths: BTreeSet<PathBuf> = roots.iter().cloned().collect();
        let mut optional_files = BTreeSet::new();
        let file_watch = matches!(
            h,
            Harness::Cursor | Harness::Goose | Harness::Hermes | Harness::Zed | Harness::Devin | Harness::OpenCode
        );
        for r in refs {
            if let Some(metadata) = crate::source::cursor_cli_metadata(r) {
                optional_files.insert(metadata.clone());
                paths.insert(metadata);
            }
            let sqlite_watch =
                file_watch || (h == Harness::OpenClaw && r.path.extension().and_then(|e| e.to_str()) == Some("sqlite"));
            if sqlite_watch {
                paths.insert(r.path.clone());
            }
            if sqlite_watch && crate::source::is_sqlite_source(r).unwrap_or(true) {
                let mut wal = r.path.as_os_str().to_os_string();
                wal.push("-wal");
                let wal = PathBuf::from(wal);
                optional_files.insert(wal.clone());
                paths.insert(wal);
            }
            let root = roots.iter().map(PathBuf::as_path).find(|rt| r.path.starts_with(rt));
            let mut d = r.path.parent();
            while let Some(dir) = d {
                paths.insert(dir.to_path_buf());
                match root {
                    // Still strictly below the root: keep walking up.
                    Some(rt) if dir != rt && dir.starts_with(rt) => d = dir.parent(),
                    // Reached the root, or the path lives outside it: stop.
                    _ => break,
                }
            }
        }
        let now = SystemTime::now();
        paths
            .into_iter()
            .filter_map(|p| {
                let md = match std::fs::metadata(&p) {
                    Ok(md) => md,
                    Err(e) if optional_files.contains(&p) && e.kind() == std::io::ErrorKind::NotFound => {
                        return Some((p, WATCH_ABSENT));
                    }
                    Err(_) => return Some((p, 0)), // unknown: never record as fresh
                };
                let Ok(mtime) = md.modified() else { return Some((p, 0)) };
                let ns = mtime.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64;
                let ns = match now.duration_since(mtime) {
                    Ok(age) if age >= WATCH_FUDGE => ns,
                    _ => 0, // too fresh (or clock skew): force one re-discovery next probe
                };
                Some((p, ns))
            })
            .collect()
    }

    /// Stat-only freshness across watched directories/DB sidecars, every primary source,
    /// and newly installed roots. A shared Probe deduplicates primary paths and compares full
    /// identities, including inode/ctime on Unix. Missing or legacy primary baselines are stale.
    /// Unknown new noncached sources bootstrap through one later scoped discovery; the baseline
    /// must precede that read. SQLite failure returns None for full-discovery fallback.
    ///
    /// A first session in a previously empty deeper directory can still escape the directory
    /// watch set until the time backstop. Primary metadata checks do not hash file contents.
    pub(super) fn probe_stale_in(conn: &Connection) -> Option<Vec<Harness>> {
        let mut stale: Vec<Harness> = Vec::new();
        fn mark(stale: &mut Vec<Harness>, h: Harness) {
            if !stale.contains(&h) {
                stale.push(h);
            }
        }

        // 1. watched dirs + db files.
        let mut watched_harnesses: HashSet<Harness> = HashSet::new();
        let mut watched_paths: HashSet<(Harness, String)> = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT harness, path, mtime_ns FROM watched").ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })
                .ok()?;
            for row in rows {
                let (h, path, recorded) = row.ok()?;
                let Some(h) = Harness::parse(&h) else { continue };
                watched_harnesses.insert(h);
                watched_paths.insert((h, path.clone()));
                if stale.contains(&h) {
                    continue; // already condemned; skip the stat
                }
                if !watch_is_fresh(Path::new(&path), recorded) {
                    mark(&mut stale, h); // changed, vanished, or fudge-stamped
                }
            }
        }

        // Every primary source, with one identity-aware metadata probe shared across paths.
        {
            let mut stmt = conn.prepare("SELECT DISTINCT s.harness,s.path,p.reads FROM sessions s LEFT JOIN primary_sources p ON p.harness=s.harness AND p.path=s.path").ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .ok()?;
            let mut probe = crate::source::Probe::default();
            for row in rows {
                let (h, _, json) = row.ok()?;
                let Some(h) = Harness::parse(&h) else { continue };
                if stale.contains(&h) {
                    continue;
                }
                let reads = json.and_then(|s| serde_json::from_str::<crate::source::ReadSet>(&s).ok());
                if !reads.is_some_and(|reads| reads.fresh(&mut probe)) {
                    mark(&mut stale, h);
                }
            }
        }

        // 3. harnesses that appeared (root now exists, no watch rows) or vanished outright, and
        //    existing roots with no watch row of their own (appeared since the last sync).
        for a in crate::harness::all() {
            let h = a.harness();
            if stale.contains(&h) {
                continue;
            }
            let unwatched_root = || {
                a.roots()
                    .iter()
                    .any(|r| r.exists() && !watched_paths.contains(&(h, r.to_string_lossy().into_owned())))
            };
            if !a.roots().is_empty() != watched_harnesses.contains(&h) || unwatched_root() {
                mark(&mut stale, h);
            }
        }

        Some(stale)
    }

    /// Every catalog row, in insertion (= discovery) order. Full publication preserves rowids
    /// only when its complete ordered refs match; otherwise it reinserts successful harnesses
    /// in discovery order. Thus after a full sync `rowid`
    /// order reproduces [`crate::discover_all`]'s output order exactly — which keeps `cv ls`'s
    /// stable sorts byte-identical even across rows whose timestamps tie. `None` on any sqlite
    /// error — callers fall back to a full discovery. Note `Some(vec![])` is a valid answer (a
    /// machine with no sessions); cold-vs-empty is [`Reader::last_full_sync`]'s job.
    pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
        Reader::open()?.all_sessions()
    }

    pub(super) fn all_sessions_in(conn: &Connection) -> Option<Vec<SessionRef>> {
        let mut stmt = conn
            .prepare(&format!("SELECT {REF_COLUMNS} FROM sessions ORDER BY rowid"))
            .ok()?;
        let rows = stmt.query_map([], row_to_ref).ok()?;
        rows.filter_map(Result::transpose)
            .collect::<rusqlite::Result<Vec<_>>>()
            .ok()
    }

    /// Seconds-since-epoch of the last completed full discovery, if any. Absent = cold catalog.
    /// Consume the notice a discovery-version upgrade left; exactly one caller sees `true`.
    pub(super) fn take_rediscovery_notice_in(conn: &Connection) -> bool {
        conn.execute("DELETE FROM catalog_meta WHERE key='rediscovery_notice'", [])
            .is_ok_and(|n| n > 0)
    }

    pub(super) fn last_full_sync_in(conn: &Connection) -> Option<i64> {
        conn.query_row("SELECT value FROM catalog_meta WHERE key='last_full_sync'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()?
        .parse()
        .ok()
    }

    #[cfg(test)]
    fn last_full_sync() -> Option<i64> {
        Reader::open()?.last_full_sync()
    }

    /// Stamp "a full discovery just completed" (the [`Reader::last_full_sync`] the staleness backstop reads).
    fn stamp_full_sync_in(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync',?1)",
            params![chrono::Utc::now().timestamp().to_string()],
        )?;
        Ok(())
    }

    /// A full parse counted `n` turns in `r`: write it into `r`'s catalog row and its scan-cache
    /// entry where they hold `0` ("not counted"), so listings show it and the next discovery (a
    /// scan-cache hit while the file is unchanged) keeps it. A row with a count is never touched:
    /// discovery's number stays the authority wherever it has one. Never creates the catalog.
    pub fn fill_count(r: &SessionRef, n: usize) {
        if !db_path().is_some_and(|p| p.exists()) {
            return;
        }
        let Some(conn) = open() else { return };
        fill_count_in(&conn, r, n);
    }

    pub(crate) fn fill_count_in(conn: &Connection, r: &SessionRef, n: usize) {
        let path = r.path.to_string_lossy();
        let _ = conn.execute(
            "UPDATE sessions SET message_count=?1
             WHERE harness=?2 AND id=?3 AND path=?4 AND message_count=0",
            params![n as i64, r.harness.as_str(), r.id, path],
        );
        crate::discover_cache::fill_count(conn, &r.path, r.harness, &r.id, n);
    }

    /// `(harness, id, path)` → the captured primary source tuple and counts a full read recorded.
    type AffordanceRows = std::collections::HashMap<(String, String, String), (i64, i64, String)>;

    /// Legacy aggregate signature retained for API compatibility. Fresh affordance counts use
    /// the captured primary tuple plus exact auxiliary dependencies instead: directory totals
    /// and a shared database session's catalog timestamp cannot establish those dependencies.
    pub fn affordance_sig(r: &SessionRef) -> (i64, i64) {
        let Ok(md) = std::fs::metadata(&r.path) else {
            return (0, 0);
        };
        let shared_db = r
            .path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3" | "vscdb"));
        if shared_db {
            return (r.updated_at.and_then(|t| t.timestamp_nanos_opt()).unwrap_or(0), -1);
        }
        if md.is_dir() {
            let (mut newest, mut total) = (0i64, 0i64);
            for e in std::fs::read_dir(&r.path).into_iter().flatten().flatten() {
                let Ok(m) = e.metadata() else { continue };
                if m.is_file() {
                    let t = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_nanos() as i64);
                    newest = newest.max(t);
                    total += m.len() as i64;
                }
            }
            return (newest, total);
        }
        crate::offsets::file_sig(&r.path)
    }

    /// The key under which a counts row records the [`crate::affordance::VIEW_VERSION`] that
    /// counted it.
    const VERSION_KEY: &str = "_v";

    fn counts_text(c: &crate::affordance::Counts) -> String {
        let mut obj: serde_json::Map<String, serde_json::Value> = c
            .iter()
            .map(|(k, n)| (k.as_str().to_string(), serde_json::json!(n)))
            .collect();
        obj.insert(VERSION_KEY.into(), crate::affordance::VIEW_VERSION.into());
        serde_json::Value::Object(obj).to_string()
    }

    /// The counts a row holds, or `None` when an older view counted them (a new kind or a new
    /// reader could have found more): such a row is "not counted", and the next full read
    /// rewrites it.
    pub(super) fn counts_of(text: &str) -> Option<crate::affordance::Counts> {
        let Ok(serde_json::Value::Object(o)) = serde_json::from_str(text) else {
            return None;
        };
        if o.get(VERSION_KEY).and_then(serde_json::Value::as_u64) != Some(crate::affordance::VIEW_VERSION.into()) {
            return None;
        }
        let mut counts = crate::affordance::Counts::new();
        for (key, value) in &o {
            if let Some(kind) = crate::affordance::Kind::parse(key) {
                // A malformed known count is a cache miss, not an absent affordance or a
                // wrapped zero. Unknown extension keys remain forward compatible.
                counts.insert(kind, u32::try_from(value.as_u64()?).ok()?);
            }
        }
        Some(counts)
    }

    /// Compatibility entry point without a before-read source snapshot. It cannot publish fresh
    /// counts safely: the closure may contain counts from an earlier read. Use
    /// [`note_affordances_with_source`] with a snapshot captured before counting instead.
    #[deprecated(note = "no-op without a before-read source snapshot; use note_affordances_with_source")]
    pub fn note_affordances(_r: &SessionRef, _counts: impl FnOnce() -> crate::affordance::Counts) {}

    /// Record counts against the dependencies captured before their complete source pass.
    pub fn note_affordances_with_source(
        r: &SessionRef,
        source: &crate::source::Snapshot,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        if !db_path().is_some_and(|p| p.exists()) {
            return;
        }
        if !crate::source::unchanged(r, source) {
            return;
        }
        if source.primary.0 == 0 {
            return;
        }
        let Some(mut conn) = open() else { return };
        note_affordances_in(&mut conn, r, source, counts);
    }

    /// The caller verified the captured source before publishing on this existing connection.
    /// The output and its independent freshness baseline still commit in one transaction.
    pub(crate) fn note_affordances_in(
        conn: &mut Connection,
        r: &SessionRef,
        source: &crate::source::Snapshot,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        let (a, b) = source.primary;
        let path = r.path.to_string_lossy();
        let stored: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT sig_a, sig_b, counts FROM affordance_counts WHERE harness=?1 AND id=?2 AND path=?3",
                params![r.harness.as_str(), r.id, path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        if stored.is_some_and(|(sa, sb, text)| (sa, sb) == (a, b) && counts_of(&text).is_some())
            && crate::source::fresh_in(conn, "counts", r, source.primary, &source.dependencies)
        {
            return;
        }
        let Ok(tx) = conn.transaction() else { return };
        let written = tx.execute(
            "INSERT OR REPLACE INTO affordance_counts(harness,id,path,sig_a,sig_b,counts) VALUES(?1,?2,?3,?4,?5,?6)",
            params![r.harness.as_str(), r.id, path, a, b, counts_text(&counts())],
        );
        if written.is_ok() && crate::source::publish_in(&tx, "counts", r, source).is_some() {
            let _ = tx.commit();
        }
    }

    /// Every recorded affordance count, loaded once for a sweep (`cv ls -q has:plan`, `cv index`).
    pub struct AffordanceTable {
        rows: AffordanceRows,
        dependencies: crate::source::Table,
    }

    impl AffordanceTable {
        pub fn load() -> AffordanceTable {
            let load = || -> Option<AffordanceTable> {
                let mut conn = open()?;
                let tx = conn.transaction().ok()?;
                let mut stmt = tx
                    .prepare("SELECT harness, id, path, sig_a, sig_b, counts FROM affordance_counts")
                    .ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            (row.get(0)?, row.get(1)?, row.get(2)?),
                            (row.get(3)?, row.get(4)?, row.get(5)?),
                        ))
                    })
                    .ok()?;
                Some(AffordanceTable {
                    rows: rows.flatten().collect(),
                    dependencies: crate::source::Table::load_in(&tx, "counts"),
                })
            };
            load().unwrap_or_else(|| AffordanceTable {
                rows: Default::default(),
                dependencies: Default::default(),
            })
        }

        /// `r`'s counts when a full read by this view recorded them and they are still fresh;
        /// `None` ("not counted") otherwise. Stats the session once.
        pub fn counts(&self, r: &SessionRef) -> Option<crate::affordance::Counts> {
            self.counts_with_source(r, &crate::source::Probe::default().snapshot(r))
        }

        /// Use a shared operation's dependency probe rather than stat'ing shared files again.
        pub fn counts_with_source(
            &self,
            r: &SessionRef,
            source: &crate::source::Snapshot,
        ) -> Option<crate::affordance::Counts> {
            self.counts_with_probe(r, source, &mut crate::source::Probe::default())
        }

        pub fn counts_with_probe(
            &self,
            r: &SessionRef,
            source: &crate::source::Snapshot,
            probe: &mut crate::source::Probe,
        ) -> Option<crate::affordance::Counts> {
            let key = (
                r.harness.as_str().to_string(),
                r.id.clone(),
                r.path.to_string_lossy().into_owned(),
            );
            let (a, b, text) = self.rows.get(&key)?;
            if (*a, *b) == source.primary
                && *a != 0
                && self
                    .dependencies
                    .fresh_with_probe(r, source.primary, &source.dependencies, probe)
            {
                counts_of(text)
            } else {
                None
            }
        }

        /// Whether `r` has no fresh counts (a sweep re-reads it to fill them).
        pub fn stale(&self, r: &SessionRef) -> bool {
            self.counts(r).is_none()
        }
    }

    /// UTF-8 lexicographic successor of a literal prefix for SQLite's binary text index.
    /// A suffix of maximal scalars carries into the preceding scalar; empty/all-maximal
    /// prefixes need no upper bound. Skip the surrogate gap, which Rust strings cannot contain.
    fn prefix_end(prefix: &str) -> Option<String> {
        for (offset, ch) in prefix.char_indices().rev() {
            if ch != char::MAX {
                let next = if ch == '\u{d7ff}' {
                    '\u{e000}'
                } else {
                    char::from_u32(ch as u32 + 1)?
                };
                let mut end = prefix[..offset].to_owned();
                end.push(next);
                return Some(end);
            }
        }
        None
    }

    pub(super) fn exact_session_in(
        conn: &Connection,
        id: &str,
        harness: Option<Harness>,
        overrides: [bool; 2],
    ) -> Option<SessionRef> {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {REF_COLUMNS} FROM sessions WHERE id=?1 AND (?2 IS NULL OR harness=?2) ORDER BY rowid"
            ))
            .ok()?;
        let mut rows = stmt
            .query_map(params![id, harness.map(Harness::as_str)], |row| {
                row_to_ref_projected(row, overrides)
            })
            .ok()?;
        // Unknown newer harness rows are skipped, as in lookup; the first known exact copy wins.
        rows.find_map(|row| row.ok().flatten())
    }

    /// All catalog rows whose id equals `id` or starts with it, optionally constrained to one
    /// harness. Order: exact-id matches first. Empty on any error or a cold catalog.
    pub fn lookup(id: &str, harness: Option<Harness>) -> Vec<SessionRef> {
        let Some(conn) = open() else { return Vec::new() };
        let upper = prefix_end(id);
        // Binary prefix ranges use idx_sessions_id; LIKE's case-folded comparison would scan
        // the entire table despite that index, and disagree with literal Rust ID matching.
        let range = if upper.is_some() {
            "id>=?1 AND id<?2"
        } else {
            "id>=?1 AND ?2 IS NULL"
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {REF_COLUMNS} FROM sessions
             WHERE {range} AND (?3 IS NULL OR harness=?3)
             ORDER BY (id=?1) DESC",
        )) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![id, upper, harness.map(Harness::as_str)], row_to_ref);
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten().flatten().filter(|r| r.id.starts_with(id)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn db_ref(harness: Harness, path: PathBuf) -> SessionRef {
            SessionRef {
                harness,
                id: "session".into(),
                path,
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: 0,
            }
        }

        #[test]
        fn search_rows_share_one_exact_catalog_reader_and_keep_first_copy_metadata() {
            let w = crate::test_world("search-row-catalog-owner");
            let first_path = w.base.join("first");
            let newer_path = w.base.join("newer");
            std::fs::write(&first_path, "old").unwrap();
            std::fs::write(&newer_path, "newer").unwrap();
            let mut first = db_ref(Harness::Claude, first_path.clone());
            first.id = "hit".into();
            first.message_count = 7;
            let mut newer = first.clone();
            newer.path = newer_path;
            newer.message_count = 9;
            newer.updated_at = chrono::DateTime::from_timestamp(1_700_000_000, 0);
            let mut prefix = first.clone();
            prefix.id = "hit-longer".into();
            sync(&[first.clone(), newer, prefix]);
            let count = || OPEN_COUNT.with(std::cell::Cell::get);
            let before = count();
            let context = crate::rows::SearchRowContext::new();
            assert_eq!(count(), before, "empty response contexts do not initialize a catalog");
            let make = |id, harness| crate::rows::SearchRow {
                id,
                harness,
                cwd: Some("/indexed"),
                title: Some("indexed title"),
                created_at: None,
                updated_at: None,
                score: Some(1.0),
                snippet: "snippet",
                agent_id: None,
                parent_id: None,
                workflow: None,
            };
            let row = make("hit", "claude").to_json_with(&context);
            assert_eq!(row["path"], first_path.to_string_lossy().as_ref());
            assert_eq!(row["message_count"], 7);
            assert_eq!(row["size_bytes"], 3);
            assert_eq!(row["title"], "indexed title");
            assert_eq!(row["cwd"], "/indexed");
            assert_eq!(make("hit", "claude").to_json_with(&context), row);
            assert!(
                make("hi", "claude").to_json_with(&context)["path"].is_null(),
                "a prefix is not an indexed exact hit"
            );
            assert!(make("hit", "codex").to_json_with(&context)["path"].is_null());
            assert_eq!(
                count() - before,
                1,
                "one catalog/schema initialization for all shaped hits"
            );
            assert_eq!(
                lookup("hit", Some(Harness::Claude))
                    .into_iter()
                    .find(|r| r.id == "hit")
                    .unwrap()
                    .path,
                first.path
            );
            assert_eq!(
                row.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
                crate::rows::SEARCH_ROW_KEYS
            );
        }

        #[test]
        fn search_projection_omits_overridden_hints_but_keeps_fallback_and_invalid_copy_policy() {
            let w = crate::test_world("search-row-projection");
            let mut first = db_ref(Harness::Claude, w.base.join("first"));
            first.title = Some("catalog custom title".repeat(4096));
            first.cwd = Some(PathBuf::from("/catalog/path".repeat(4096)));
            first.created_at = chrono::DateTime::from_timestamp(1_700_000_000, 0);
            let mut second = first.clone();
            second.path = w.base.join("second");
            second.message_count = 9;
            sync(&[first.clone(), second]);
            let conn = open().unwrap();
            let projected = exact_session_in(&conn, "session", Some(Harness::Claude), [true, true]).unwrap();
            assert!(
                projected.title.is_none() && projected.cwd.is_none(),
                "overridden hints were owned"
            );
            let full = exact_session_in(&conn, "session", Some(Harness::Claude), [false, false]).unwrap();
            assert_eq!(full.title, first.title);
            assert_eq!(full.cwd, first.cwd);
            let row = crate::rows::SearchRow {
                id: "session",
                harness: "claude",
                title: Some("index title"),
                cwd: Some("/index"),
                created_at: Some(i64::MAX),
                updated_at: Some(0),
                score: None,
                snippet: "snippet",
                agent_id: None,
                parent_id: None,
                workflow: None,
            };
            let output = row.to_json_with(&crate::rows::SearchRowContext::new());
            assert_eq!(output["title"], "index title");
            assert_eq!(output["cwd"], "/index");
            assert_eq!(
                output["created_at"],
                first.created_at.unwrap().to_rfc3339(),
                "invalid index date lost fallback"
            );
            assert_eq!(output["updated_at"], "1970-01-01T00:00:00+00:00");
            for expression in ["X'ff'", "CAST(X'ff' AS TEXT)"] {
                conn.execute(
                    &format!("UPDATE sessions SET title={expression} WHERE path=?1"),
                    [first.path.to_string_lossy()],
                )
                .unwrap();
                let expected = lookup("session", Some(Harness::Claude))
                    .into_iter()
                    .find(|r| r.id == "session")
                    .unwrap();
                let actual = exact_session_in(&conn, "session", Some(Harness::Claude), [true, true]).unwrap();
                assert_eq!(
                    actual.path, expected.path,
                    "overridden invalid hint changed copy acceptance"
                );
                assert_eq!(actual.message_count, 9);
            }
        }

        #[test]
        fn primary_inventory_omits_metadata_and_preserves_source_selection() {
            let w = crate::test_world("catalog-primary-inventory");
            let refs: Vec<_> = crate::harness::all()
                .into_iter()
                .enumerate()
                .map(|(i, a)| {
                    let mut r = db_ref(a.harness(), w.base.join(format!("sessions/{i}/session")));
                    r.id = "session".into();
                    r.title = Some("unused title".repeat(1024));
                    r.cwd = Some(w.base.join("unused-cwd"));
                    r.message_count = 123;
                    r
                })
                .collect();
            sync(&refs);
            let reader = Reader::open().unwrap();
            let projected = reader.primary_sessions();
            assert_eq!(projected.len(), refs.len());
            for (full, lean) in refs.iter().zip(&projected) {
                assert_eq!(
                    (&full.id, full.harness, &full.path),
                    (&lean.id, lean.harness, &lean.path)
                );
                assert!(lean.title.is_none() && lean.cwd.is_none());
                assert_eq!(lean.message_count, 0);
                let a = crate::harness::for_harness(full.harness).unwrap();
                assert_eq!(a.resume_source(full), a.resume_source(lean));
                let conn = open().unwrap();
                let scoped = primary_refs_in(&conn, Some(full.harness));
                assert_eq!(scoped.len(), 1);
                assert_eq!(scoped[0].path, full.path);
            }
        }

        #[test]
        fn primary_inventory_preserves_empty_unknown_and_error_fallbacks() {
            let w = crate::test_world("catalog-primary-errors");
            let conn = open().unwrap();
            assert!(primary_refs_in(&conn, None).is_empty());
            let r = db_ref(Harness::Claude, w.base.join("session.jsonl"));
            sync(std::slice::from_ref(&r));
            // Newer binaries' unknown rows are skipped before decoding identity columns,
            // matching the full row reader even if their column representation differs.
            conn.execute(
                "INSERT INTO sessions(harness,id,path) VALUES('future-harness',X'01',X'02')",
                [],
            )
            .unwrap();
            let projected = primary_refs_in(&conn, None);
            let full = all_sessions_in(&conn).unwrap();
            assert_eq!(projected.len(), 1);
            assert_eq!(projected[0].path, full[0].path);
            assert!(primary_refs_in(&conn, Some(Harness::Codex)).is_empty());
            conn.execute_batch("DROP TABLE sessions").unwrap();
            assert!(primary_refs_in(&conn, None).is_empty());
            assert!(primary_refs_in(&conn, Some(Harness::Claude)).is_empty());
        }

        #[test]
        fn lookup_matches_literal_case_sensitive_prefixes() {
            let w = crate::test_world("catalog-literal-prefix");
            let ids = [
                "Alpha",
                "alpha",
                "a_%\\literal",
                "aXYliteral",
                "λ_session",
                "λ_other",
                "a\0one",
                "a\0two",
                "a\u{d7ff}tail",
                "a\u{e000}tail",
                "a\u{10ffff}tail",
                "\u{10ffff}tail",
                "\u{10ffff}\u{10ffff}tail",
            ];
            let refs: Vec<_> = ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| {
                    let mut r = db_ref(Harness::Claude, w.base.join(format!("{i}.jsonl")));
                    r.id = id.into();
                    r
                })
                .collect();
            let mut foreign = refs[0].clone();
            foreign.harness = Harness::Codex;
            foreign.path = w.base.join("foreign.jsonl");
            sync(&refs);
            sync(&[foreign]);
            for prefix in [
                "A",
                "a",
                "a_%\\",
                "λ_",
                "λ_s",
                "",
                "a\0",
                "a\u{d7ff}",
                "a\u{e000}",
                "a\u{10ffff}",
                "\u{10ffff}",
                "\u{10ffff}\u{10ffff}",
                "missing",
            ] {
                let actual: std::collections::BTreeSet<_> = lookup(prefix, Some(Harness::Claude))
                    .into_iter()
                    .map(|r| r.id)
                    .collect();
                let expected = refs
                    .iter()
                    .filter(|r| r.id.starts_with(prefix))
                    .map(|r| r.id.clone())
                    .collect();
                assert_eq!(actual, expected, "prefix {prefix:?}");
            }
            assert_eq!(lookup("Alpha", None).len(), 2);
        }

        #[test]
        fn reader_warm_sessions_initialize_catalog_once() {
            let w = crate::test_world("reader-warm");
            let path = w.home.join(".claude/projects/-reader/alpha.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"alpha\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n").unwrap();
            let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for dir in path
                .parent()
                .unwrap()
                .ancestors()
                .take_while(|dir| dir.starts_with(&w.home))
            {
                std::fs::File::open(dir)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
            let discovered = crate::discover_all();
            assert_eq!(discovered.len(), 1);
            let count = || OPEN_COUNT.with(std::cell::Cell::get);
            let before = count();
            assert!(Reader::open().unwrap().last_full_sync().is_some());
            assert!(Reader::open().unwrap().probe_stale().unwrap().is_empty());
            let legacy = Reader::open().unwrap().all_sessions().unwrap();
            assert_eq!(count() - before, 3, "prior three-open read recipe");
            let before = count();
            let (current, full) = crate::sessions_impl();
            assert!(!full, "verified catalog read must stay on the warm route");
            assert_eq!(
                count() - before,
                1,
                "one owned operation connection/schema initialization"
            );
            assert_eq!(
                serde_json::to_value(current).unwrap(),
                serde_json::to_value(legacy).unwrap()
            );
            // Initial discovery has already warmed the process-local metadata cache. A first-use
            // scan remains demand-lazy and may independently open it; SQLite-only refreshes must
            // not bulk-load unrelated transcript metadata just to save an initialization.
            use std::io::Write;
            writeln!(std::fs::OpenOptions::new().append(true).open(&path).unwrap(),
                "{{\"type\":\"user\",\"uuid\":\"u2\",\"sessionId\":\"alpha\",\"message\":{{\"role\":\"user\",\"content\":\"second\"}}}}"
            ).unwrap();
            let before = count();
            let (refreshed, full) = crate::sessions_impl();
            assert!(!full, "one changed primary uses scoped refresh");
            assert_eq!(
                count() - before,
                1,
                "capture, publication and cache persist reuse the operation connection"
            );
            assert_eq!(refreshed[0].message_count, 2);
            assert_eq!(
                serde_json::to_value(&refreshed).unwrap(),
                serde_json::to_value(crate::discover_all()).unwrap()
            );
        }

        #[test]
        fn reader_operations_follow_catalog_switch_and_replacement() {
            let w = crate::test_world("reader-path");
            let a = db_ref(Harness::Claude, w.base.join("first"));
            sync(std::slice::from_ref(&a));
            assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].path, a.path);
            std::fs::rename(w.cv_home.join("catalog.db"), w.cv_home.join("old-catalog.db")).unwrap();
            let b = db_ref(Harness::Claude, w.base.join("replacement"));
            sync(std::slice::from_ref(&b));
            assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].path, b.path);
            let next = w.base.join("next-catalog");
            w.scope.set("CLUSTERVISION_HOME", &next);
            let c = db_ref(Harness::Claude, w.base.join("switched"));
            sync(std::slice::from_ref(&c));
            assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].path, c.path);
        }

        #[test]
        fn reader_statement_errors_remain_fallbacks_without_held_transactions() {
            let _w = crate::test_world("reader-errors");
            let reader = Reader::open().unwrap();
            let conn = open().unwrap();
            // Another writer can change schema while this operation owner is idle.
            conn.execute_batch("BEGIN IMMEDIATE; DROP TABLE watched; COMMIT;")
                .unwrap();
            assert!(reader.probe_stale().is_none());
            conn.execute_batch("DROP TABLE catalog_meta;").unwrap();
            assert!(reader.last_full_sync().is_none());
            conn.execute_batch("DROP TABLE sessions;").unwrap();
            assert!(reader.all_sessions().is_none());
        }

        #[test]
        fn reader_preload_does_not_capture_primary_identity_early() {
            let w = crate::test_world("reader-before-source");
            let path = w.base.join("session.jsonl");
            std::fs::write(&path, "before").unwrap();
            let r = db_ref(Harness::Claude, path.clone());
            sync(std::slice::from_ref(&r));
            let known = Reader::open().unwrap().all_sessions().unwrap();
            std::fs::write(&path, "loaded").unwrap();
            let adapter = crate::harness::for_harness(Harness::Claude).unwrap();
            let captured = capture_primary_reads_from(adapter.as_ref(), &known);
            assert!(captured[&path].fresh(&mut crate::source::Probe::default()));
            std::fs::write(&path, "racing").unwrap();
            assert!(!captured[&path].fresh(&mut crate::source::Probe::default()));
        }

        #[test]
        fn publication_preserves_full_tie_order_and_failed_harness_rows() {
            let w = crate::test_world("publication-order");
            let mut a = db_ref(Harness::Claude, w.base.join("a"));
            a.id = "a".into();
            let mut b = db_ref(Harness::Gemini, w.base.join("b"));
            b.id = "b".into();
            let mut failed = db_ref(Harness::Devin, w.base.join("failed"));
            failed.id = "failed".into();
            sync(&[a.clone(), b.clone()]);
            assert!(matches_sessions([&a, &b].into_iter()));
            assert!(!matches_sessions([&b, &a].into_iter()), "ties retain adapter order");
            let before = PrimaryReads::default();
            let publish = |bs: &[SessionRef]| {
                publish_all(&[
                    (Harness::Claude, std::slice::from_ref(&a), Vec::new(), &before),
                    (Harness::Gemini, bs, Vec::new(), &before),
                ])
            };
            b.message_count = 1;
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&a, &b].into_iter()));
            // Intervening additive publication after enumeration must defeat the skip.
            let mut intervening = b.clone();
            intervening.message_count = 909;
            sync(&[intervening]);
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&a, &b].into_iter()));
            sync(std::slice::from_ref(&failed));
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&failed, &a, &b].into_iter()));
            let mut new = b.clone();
            new.id = "new".into();
            publish(&[b.clone(), new]);
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&failed, &a, &b].into_iter()));
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO sessions(harness,id,path,message_count) VALUES('future-harness','future','future',0)",
                [],
            )
            .unwrap();
            publish(std::slice::from_ref(&b));
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM sessions WHERE harness='future-harness'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
        }

        #[test]
        fn publication_failure_rolls_back_rows_guards_and_full_sync_stamp() {
            let w = crate::test_world("publication-abort");
            let path = w.base.join("session.jsonl");
            std::fs::write(&path, "before").unwrap();
            let a = db_ref(Harness::Claude, path.clone());
            let mut before = PrimaryReads::default();
            let mut reads = crate::source::ReadSet::default();
            reads.record(&path);
            before.insert(path.clone(), reads.clone());
            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&a),
                vec![(path.clone(), 1)],
                &before,
            )]);
            let conn = open().unwrap();
            conn.execute("UPDATE catalog_meta SET value='1' WHERE key='last_full_sync'", [])
                .unwrap();
            let guard: String = conn
                .query_row(
                    "SELECT reads FROM primary_sources WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get(0),
                )
                .unwrap();
            let mut changed = a.clone();
            changed.message_count = 2;
            std::fs::write(&path, "after!").unwrap();
            let mut newer = crate::source::ReadSet::default();
            newer.record(&path);
            before.insert(path.clone(), newer);
            conn.execute_batch("CREATE TRIGGER reject_publication BEFORE INSERT ON sessions BEGIN SELECT RAISE(ABORT,'publication'); END;").unwrap();
            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&changed),
                vec![(path.clone(), 2)],
                &before,
            )]);
            assert!(matches_sessions(std::iter::once(&a)));
            assert_eq!(last_full_sync(), Some(1));
            let unchanged: String = conn
                .query_row(
                    "SELECT reads FROM primary_sources WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(unchanged, guard);
            assert_eq!(
                conn.query_row(
                    "SELECT mtime_ns FROM watched WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
            conn.execute_batch("DROP TRIGGER reject_publication;").unwrap();
            // Restoring original captured proof must not bless the intervening file rewrite.
            before.insert(path.clone(), reads.clone());
            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&changed),
                vec![(path.clone(), 2)],
                &before,
            )]);
            let published: String = conn
                .query_row(
                    "SELECT reads FROM primary_sources WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<crate::source::ReadSet>(&published).unwrap(),
                reads
            );
            assert!(!reads.fresh(&mut crate::source::Probe::default()));
        }

        #[test]
        fn arbitrary_opencode_database_names_watch_native_wal() {
            let w = crate::test_world("catalog-opencode-arbitrary-db");
            let path = w.base.join("custom-store");
            std::fs::write(&path, b"SQLite format 3\0native database bytes").unwrap();
            let r = db_ref(Harness::OpenCode, path.clone());
            let watches = watches_for(r.harness, std::slice::from_ref(&w.base), std::slice::from_ref(&r));
            assert!(watches.iter().any(|(p, _)| p == &w.base.join("custom-store-wal")));
            let legacy = w.base.join("ses_old.json");
            std::fs::write(&legacy, "{}").unwrap();
            let r = db_ref(Harness::OpenCode, legacy);
            assert!(!watches_for(r.harness, &[], &[r])
                .iter()
                .any(|(p, _)| p == &w.base.join("ses_old.json-wal")));
        }

        #[test]
        fn watches_track_cursor_cli_metadata_without_database_changes() {
            let world = crate::test_world("catalog-cursor-cli-meta");
            let path = world.base.join("store.db");
            std::fs::write(&path, []).unwrap();
            let mut r = db_ref(Harness::Cursor, path);
            r.id = "cli:session".into();
            let metadata = world.base.join("meta.json");
            let watches = watches_for(Harness::Cursor, std::slice::from_ref(&world.base), &[r]);
            let absent = watches.iter().find(|(path, _)| path == &metadata).unwrap().1;
            assert_eq!(absent, WATCH_ABSENT);
            assert!(watch_is_fresh(&metadata, absent));
            std::fs::write(&metadata, r#"{"cwd":"/scratch/project"}"#).unwrap();
            assert!(!watch_is_fresh(&metadata, absent));
            let present = crate::offsets::file_sig(&metadata).0;
            assert!(watch_is_fresh(&metadata, present));
            std::fs::File::open(&metadata)
                .unwrap()
                .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(123))
                .unwrap();
            assert!(!watch_is_fresh(&metadata, present));
            std::fs::remove_file(&metadata).unwrap();
            assert!(!watch_is_fresh(&metadata, present));
        }

        #[test]
        fn watches_track_absent_wal_and_its_appearance_and_removal() {
            let w = crate::test_world("catalog-absent-wal");
            let path = w.base.join("openclaw-agent.sqlite");
            std::fs::write(&path, []).unwrap();
            let wal = w.base.join("openclaw-agent.sqlite-wal");
            let r = db_ref(Harness::OpenClaw, path.clone());
            let watches = watches_for(Harness::OpenClaw, std::slice::from_ref(&w.base), &[r]);
            assert!(
                watches.iter().any(|(p, _)| *p == path),
                "OpenClaw SQLite is file-watched"
            );
            let absent = watches.iter().find(|(p, _)| *p == wal).unwrap().1;
            assert_eq!(absent, WATCH_ABSENT);
            assert!(watch_is_fresh(&wal, absent));

            std::fs::write(&wal, "wal").unwrap();
            assert!(
                !watch_is_fresh(&wal, absent),
                "WAL appearance invalidates its absent watch"
            );
            let present = crate::offsets::file_sig(&wal).0;
            assert!(watch_is_fresh(&wal, present));
            std::fs::remove_file(&wal).unwrap();
            assert!(
                !watch_is_fresh(&wal, present),
                "WAL removal invalidates its present watch"
            );
            assert!(!watch_is_fresh(&wal, 0), "unknown is not stable absence");

            let jsonl = db_ref(Harness::OpenClaw, w.base.join("legacy.jsonl"));
            let watches = watches_for(Harness::OpenClaw, &[], std::slice::from_ref(&jsonl));
            assert!(
                !watches.iter().any(|(p, _)| *p == jsonl.path),
                "legacy JSONL keeps the bounded file probe"
            );
        }

        #[test]
        fn wal_only_write_invalidates_database_watch_set() {
            let w = crate::test_world("catalog-live-wal");
            let path = w.base.join("sessions.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE messages(text TEXT);",
            )
            .unwrap();
            let wal = w.base.join("sessions.db-wal");
            // Age the files so the test exercises actual WAL freshness, not WATCH_FUDGE.
            let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for p in [&path, &wal, &w.base] {
                std::fs::File::open(p)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
            let db_sig = crate::offsets::file_sig(&path);
            let r = db_ref(Harness::OpenCode, path.clone());
            let watches = watches_for(Harness::OpenCode, std::slice::from_ref(&w.base), &[r]);
            assert!(watches.iter().all(|(p, stamp)| watch_is_fresh(p, *stamp)));
            conn.execute("INSERT INTO messages VALUES ('new message')", []).unwrap();
            assert_eq!(crate::offsets::file_sig(&path), db_sig, "write stayed in WAL");
            let changed: Vec<_> = watches.iter().filter(|(p, stamp)| !watch_is_fresh(p, *stamp)).collect();
            assert_eq!(changed.len(), 1);
            assert_eq!(
                changed[0].0, wal,
                "only WAL invalidates the otherwise unchanged watch set"
            );
        }

        /// A counts row an older view wrote (no version, or another one) is "not counted": a kind
        /// that view did not know must not read as absent.
        #[test]
        fn malformed_known_counts_are_cache_misses_without_integer_wrapping() {
            for value in [
                serde_json::json!(4294967296_u64),
                serde_json::json!(-1),
                serde_json::json!("1"),
                serde_json::Value::Null,
            ] {
                let text = serde_json::json!({"_v":crate::affordance::VIEW_VERSION, "approval":value}).to_string();
                assert!(counts_of(&text).is_none(), "{text}");
            }
            let valid = serde_json::json!({"_v":crate::affordance::VIEW_VERSION, "approval":u32::MAX, "future_kind":"unknown extension"}).to_string();
            assert_eq!(counts_of(&valid).unwrap()[&crate::affordance::Kind::Approval], u32::MAX);
        }

        #[test]
        fn counts_an_older_view_wrote_are_not_counted() {
            let mut c = crate::affordance::Counts::new();
            c.insert(crate::affordance::Kind::Approval, 2);
            assert_eq!(counts_of(&counts_text(&c)), Some(c));
            assert_eq!(counts_of(r#"{"plan": 1}"#), None);
            assert_eq!(counts_of(r#"{"plan": 1, "_v": 1}"#), None);
            // Classifier changes can turn a previous view's zero into a positive count.
            // Reject that row so both has:approval and its negation trigger a recount.
            let previous = serde_json::json!({
                "approval": 0,
                "_v": crate::affordance::VIEW_VERSION - 1,
            });
            assert_eq!(counts_of(&previous.to_string()), None);
            // View4's Claude Facts projection omitted generic restorable checkpoint details.
            assert_eq!(counts_of(r#"{"checkpoint":0,"_v":4}"#), None);
        }
    }
}

#[cfg(feature = "sqlite")]
#[allow(deprecated)]
pub use imp::{
    affordance_sig, fill_count, lookup, note_affordances, note_affordances_with_source, sync, AffordanceTable,
};
#[cfg(feature = "sqlite")]
pub(crate) use imp::{all_sessions, db_path, open_at, open_db, watches_for};

#[cfg(all(feature = "sqlite", test))]
pub(crate) use imp::replace_harness;

#[cfg(all(feature = "sqlite", test))]
pub(crate) fn last_full_sync() -> Option<i64> {
    Reader::open()?.last_full_sync()
}

/// No-op fallbacks when sqlite is disabled — `find`/`sessions` simply always take the scan path.
#[cfg(not(feature = "sqlite"))]
pub fn sync(_refs: &[SessionRef]) {}
#[cfg(not(feature = "sqlite"))]
pub fn fill_count(_r: &SessionRef, _n: usize) {}
#[cfg(not(feature = "sqlite"))]
#[deprecated(note = "no-op without a before-read source snapshot; use note_affordances_with_source")]
pub fn note_affordances(_r: &SessionRef, _counts: impl FnOnce() -> crate::affordance::Counts) {}
#[cfg(not(feature = "sqlite"))]
pub fn note_affordances_with_source(
    _r: &SessionRef,
    _source: &crate::source::Snapshot,
    _counts: impl FnOnce() -> crate::affordance::Counts,
) {
}
/// Without a catalog nothing is ever counted: every `has:<kind>` parses.
#[cfg(not(feature = "sqlite"))]
pub struct AffordanceTable;
#[cfg(not(feature = "sqlite"))]
impl AffordanceTable {
    pub fn load() -> AffordanceTable {
        AffordanceTable
    }
    pub fn counts(&self, _r: &SessionRef) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn counts_with_source(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Snapshot,
    ) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn counts_with_probe(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Snapshot,
        _probe: &mut crate::source::Probe,
    ) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn stale(&self, _r: &SessionRef) -> bool {
        false
    }
}
#[cfg(not(feature = "sqlite"))]
pub fn lookup(_id: &str, _harness: Option<Harness>) -> Vec<SessionRef> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn watches_for(
    _h: Harness,
    _roots: &[std::path::PathBuf],
    _refs: &[SessionRef],
) -> Vec<(std::path::PathBuf, i64)> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
    None
}

#[cfg(all(test, feature = "sqlite"))]
mod counts_tests;
