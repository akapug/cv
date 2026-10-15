//! SQLite plumbing for the database-backed stores (Cursor, Devin, Goose, Hermes, OpenClaw,
//! OpenCode, Zed). One invariant lives here so no adapter can get it wrong: **cv never writes a
//! harness's store**, not even its coordination files. Opening `SQLITE_OPEN_READ_ONLY` is not
//! enough for that: a WAL reader records its read mark in the `-shm` wal-index (and creates
//! `-wal`/`-shm` when they are missing), which rewrites a file in the harness home. So
//! [`open_ro`] chooses from the database's sidecars:
//! - no WAL frames (`-wal` absent or empty) and no `-journal`: the original with
//!   `immutable=1`, so SQLite opens no `-wal`/`-shm` and takes no locks;
//! - otherwise: a private snapshot of the database and its `-wal`/`-journal` (never `-shm`,
//!   which SQLite rebuilds in the private directory), so committed WAL-only rows stay visible.
//!
//! This also protects a rollback-mode source that switches to WAL during an operation: an
//! ordinary read-only connection could create coordination files before the after-read check.
//!
//! Every path's identity is captured before and checked after each read ([`ReadConnection::read`]),
//! so a harness write during a read is an error, not a torn result.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::Path;

#[cfg(test)]
pub(crate) mod export_tests;
mod snapshots;

/// A reader retains both its private backing (when needed) and the original provenance.
/// Use `read` around an adapter operation, and carry `reads()` into lazy source captures.
pub(crate) struct ReadConnection {
    connection: Connection,
    reads: crate::source::ReadSet,
    _snapshot: Option<std::sync::Arc<ColdSnapshot>>,
    _scope: Option<ReadScope>,
}

impl std::ops::Deref for ReadConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl ReadConnection {
    pub(crate) fn reads(&self) -> &crate::source::ReadSet {
        &self.reads
    }

    pub(crate) fn read<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        check_reads(&self.reads)?;
        let result = operation(self);
        // A mutation takes precedence over a body/SQL failure too.
        check_reads(&self.reads)?;
        result
    }
}

/// Retain at an operation boundary that reads multiple sessions. Dropping the final scope or
/// reader releases cached payloads and their directories. Concurrent/nested scopes share this
/// bounded cache. Outermost scope entry/exit also reclaims marked, unlocked owners left by dead
/// processes; a fixed tiny namespace gate persists. Abrupt standalone death needs a later scope.
pub(crate) struct ReadScope {
    cache: std::sync::Arc<SnapshotCache>,
    _boundary: Option<std::sync::Arc<ReadBoundary>>,
}

struct SnapshotCache {
    state: std::sync::Mutex<SnapshotCacheState>,
    namespace: snapshots::Namespace,
    owner: std::sync::Mutex<Option<std::sync::Arc<snapshots::Owner>>>,
}

struct ReadBoundary(snapshots::Namespace);
impl Drop for ReadBoundary {
    fn drop(&mut self) {
        self.0.reap();
    }
}

thread_local! {
    // The guard may move between threads. Its last Arc drop performs exit cleanup; the
    // origin thread only retains a Weak, never a depth that must be decremented elsewhere.
    static BOUNDARY: std::cell::RefCell<std::sync::Weak<ReadBoundary>> = const { std::cell::RefCell::new(std::sync::Weak::new()) };
}

#[derive(Default)]
struct SnapshotCacheState {
    idle: std::collections::VecDeque<std::sync::Arc<ColdSnapshot>>,
    idle_bytes: u64,
    active: std::collections::HashMap<std::path::PathBuf, std::sync::Weak<ColdSnapshot>>,
}

const IDLE_BUDGET: u64 = 256 * 1024 * 1024;

impl SnapshotCacheState {
    fn discard_idle(&mut self, snapshot: &std::sync::Arc<ColdSnapshot>) {
        if let Some(index) = self.idle.iter().position(|s| std::sync::Arc::ptr_eq(s, snapshot)) {
            self.idle_bytes -= self.idle.remove(index).unwrap().copied_bytes;
        }
    }

    fn retain_idle(&mut self, snapshot: &std::sync::Arc<ColdSnapshot>, budget: u64) {
        self.discard_idle(snapshot);
        // Permit one oversized backing alone. Its next session reuses it, while selecting any
        // other database evicts it rather than allowing unbounded idle bytes.
        while !self.idle.is_empty() && self.idle_bytes.saturating_add(snapshot.copied_bytes) > budget {
            self.idle_bytes -= self.idle.pop_front().unwrap().copied_bytes;
        }
        self.idle_bytes += snapshot.copied_bytes;
        self.idle.push_back(std::sync::Arc::clone(snapshot));
    }
}

pub(crate) fn read_scope() -> ReadScope {
    let mut scope = reader_scope();
    scope._boundary = Some(operation_boundary(&scope.cache.namespace));
    scope
}

fn operation_boundary(namespace: &snapshots::Namespace) -> std::sync::Arc<ReadBoundary> {
    BOUNDARY.with(|slot| {
        if let Some(boundary) = slot.borrow().upgrade() {
            return boundary;
        }
        namespace.reap();
        let boundary = std::sync::Arc::new(ReadBoundary(namespace.clone()));
        *slot.borrow_mut() = std::sync::Arc::downgrade(&boundary);
        boundary
    })
}

fn reader_scope() -> ReadScope {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static ACTIVE: OnceLock<Mutex<Weak<SnapshotCache>>> = OnceLock::new();
    let mut active = ACTIVE
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = active.upgrade().unwrap_or_else(|| {
        let cache = Arc::new(SnapshotCache {
            state: Mutex::new(SnapshotCacheState::default()),
            namespace: snapshots::Namespace::current(),
            owner: Mutex::new(None),
        });
        *active = Arc::downgrade(&cache);
        cache
    });
    ReadScope { cache, _boundary: None }
}

struct ColdSnapshot {
    reads: crate::source::ReadSet,
    directory: std::path::PathBuf,
    directory_identity: snapshots::Identity,
    database: std::path::PathBuf,
    copied_bytes: u64,
    _owner: Option<std::sync::Arc<snapshots::Owner>>,
}

impl Drop for ColdSnapshot {
    fn drop(&mut self) {
        snapshots::remove_snapshot_directory(&self.directory, self.directory_identity);
    }
}

fn check_reads(reads: &crate::source::ReadSet) -> Result<()> {
    anyhow::ensure!(
        reads.fresh(&mut crate::source::Probe::default()),
        "SQLite source dependencies changed or unavailable"
    );
    Ok(())
}

/// Discovery over one database, cached in the discovery cache under the read set [`open_ro`]
/// captures (the file, its WAL and rollback journal; SHM is coordination state). An unchanged
/// store is neither reopened nor rescanned. `discover` must depend only on that database; its
/// errors are returned and never cached.
pub(crate) fn cached_discover(
    db: &Path,
    discover: impl FnOnce() -> Result<Vec<crate::SessionRef>>,
) -> Result<Vec<crate::SessionRef>> {
    let path = std::path::absolute(db)?;
    let mut reads = crate::source::ReadSet::default();
    reads.record(&path);
    reads.record(&sidecar(&path, "-wal"));
    reads.record(&sidecar(&path, "-journal"));
    let mut failure = None;
    let refs = crate::discover_cache::cached_scan_many_with_reads(&path, reads, || {
        discover().unwrap_or_else(|error| {
            failure = Some(error);
            Vec::new()
        })
    });
    failure.map_or(Ok(refs), Err)
}

fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

/// Open a harness database without writing anything in its directory (see the module docs).
/// Snapshots of WAL-mode stores with frames live in a private directory; an operation-local LRU
/// retains at most 256 MiB of copied DB/WAL/journal bytes, or one larger backing alone. Weak
/// lookup reuses active readers without increasing idle disk retention. `std::fs::copy` clones
/// on APFS (0.01 s for a 267 MB store) and copies elsewhere (0.26 s for the same file here).
pub(crate) fn open_ro(path: &Path) -> Result<ReadConnection> {
    open_ro_with_copy_check(path, |_| Ok(()))
}

fn open_ro_with_copy_check(path: &Path, copied: impl FnOnce(&Path) -> Result<()>) -> Result<ReadConnection> {
    use std::sync::Arc;
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut reads = crate::source::ReadSet::default();
    reads.record(&path);
    reads.record(&sidecar(&path, "-wal"));
    reads.record(&sidecar(&path, "-journal"));
    let has_frames = std::fs::metadata(sidecar(&path, "-wal")).is_ok_and(|m| m.len() > 0);
    let has_journal = sidecar(&path, "-journal").exists();
    let immutable = !has_frames && !has_journal;
    let scope = (!immutable).then(reader_scope);
    let snapshot = if let Some(scope) = scope.as_ref() {
        let mut cache = scope
            .cache
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot cache poisoned"))?;
        cache.active.retain(|_, snapshot| snapshot.strong_count() > 0);
        let existing = cache.active.get(&path).and_then(std::sync::Weak::upgrade);
        if let Some(snapshot) = existing
            .as_ref()
            .filter(|s| s.reads.fresh(&mut crate::source::Probe::default()))
        {
            cache.retain_idle(snapshot, IDLE_BUDGET);
            Some(Arc::clone(snapshot))
        } else {
            if let Some(stale) = existing {
                cache.discard_idle(&stale);
            }
            let owner = {
                let mut slot = scope
                    .cache
                    .owner
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SQLite snapshot owner poisoned"))?;
                if slot.is_none() {
                    *slot = Some(scope.cache.namespace.owner()?);
                }
                Arc::clone(slot.as_ref().unwrap())
            };
            let directory = owner.snapshot_directory()?;
            let directory_identity = snapshots::snapshot_identity(&directory)?;
            let mut snapshot = ColdSnapshot {
                reads: reads.clone(),
                database: directory.join("store.db"),
                directory,
                directory_identity,
                copied_bytes: 0,
                _owner: Some(owner),
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&snapshot.directory, std::fs::Permissions::from_mode(0o700))?;
            }
            // std::fs::copy uses clonefile on APFS and bounded OS copying elsewhere. Never copy
            // SHM: it contains live coordination state; SQLite rebuilds it only in our directory.
            snapshot.copied_bytes = std::fs::copy(&path, &snapshot.database)?;
            for suffix in ["-wal", "-journal"] {
                let original = sidecar(&path, suffix);
                if original.exists() {
                    snapshot.copied_bytes = snapshot
                        .copied_bytes
                        .checked_add(std::fs::copy(original, sidecar(&snapshot.database, suffix))?)
                        .context("SQLite snapshot byte size overflow")?;
                }
            }
            copied(&snapshot.database)?;
            check_reads(&reads)?;
            let snapshot = Arc::new(snapshot);
            cache.active.insert(path.clone(), Arc::downgrade(&snapshot));
            cache.retain_idle(&snapshot, IDLE_BUDGET);
            Some(snapshot)
        }
    } else {
        None
    };
    let backing = snapshot.as_ref().map_or(path.as_path(), |s| s.database.as_path());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = if immutable {
        Connection::open_with_flags(immutable_uri(backing), flags | OpenFlags::SQLITE_OPEN_URI)
    } else {
        Connection::open_with_flags(backing, flags)
    }
    .with_context(|| format!("opening {}", path.display()))?;
    check_reads(&reads)?;
    Ok(ReadConnection {
        connection,
        reads,
        _snapshot: snapshot,
        _scope: scope,
    })
}

/// `file:<path>?immutable=1`, with the URI's reserved characters percent-encoded.
fn immutable_uri(path: &Path) -> String {
    const RESERVED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'%').add(b'?').add(b'#').add(b' ');
    #[cfg(unix)]
    let encoded = {
        use std::os::unix::ffi::OsStrExt;
        percent_encoding::percent_encode(path.as_os_str().as_bytes(), RESERVED).to_string()
    };
    #[cfg(not(unix))]
    let encoded = {
        let text = format!("/{}", path.to_string_lossy().replace('\\', "/"));
        percent_encoding::utf8_percent_encode(&text, RESERVED).to_string()
    };
    format!("file:{encoded}?immutable=1")
}

/// Copy a snapshot's immutable TEXT/BLOB cell into an operation-owned arena using bounded
/// scratch. Table and column are adapter-owned identifiers, never user selectors. Blob access
/// may be unavailable for virtual tables; the fallback retains one encoded cell in SQLite.
pub(crate) fn append_cell(
    conn: &Connection,
    table: &str,
    column: &str,
    rowid: i64,
    arena: &super::bytes::ByteSource,
) -> Result<(u64, u64)> {
    if let Ok(mut blob) = conn.blob_open(rusqlite::DatabaseName::Main, table, column, rowid, true) {
        return arena.append(|writer| {
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = std::io::Read::read(&mut blob, &mut buffer)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buffer[..read])?;
            }
            Ok(())
        });
    }
    conn.query_row(
        &format!("SELECT {column} FROM {table} WHERE rowid = ?1"),
        [rowid],
        |row| {
            Ok(arena.append(|writer| {
                match row.get_ref(0)? {
                    rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                        writer.write_all(bytes)?
                    }
                    _ => anyhow::bail!("SQLite source cell is not TEXT or BLOB"),
                }
                Ok(())
            }))
        },
    )?
}

/// A store's JSON cells as views: rows already are the byte source. Each cell a read selects is
/// copied once into the source's operation-owned arena (an incremental read by rowid where the
/// table allows one, else the row's own bytes), validated once, and handed to the format's one
/// decoder as a [`JsonView`](crate::json::JsonView). The demand decides what the decoder
/// resolves, never which code runs.
pub(crate) struct Cells {
    source: crate::json::JsonSource,
    arena: std::sync::Arc<super::bytes::ByteSource>,
    incremental: bool,
}

/// One cell's bytes in a [`Cells`] arena.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cell(u64, u64);

impl Cells {
    /// Cells of `table` in the database at `path`, guarded by the reader's dependencies.
    pub(crate) fn new(conn: &Connection, path: &Path, reads: &crate::source::ReadSet, table: &str) -> Result<Self> {
        let source = crate::json::JsonSource::capture_with_reads(path, reads)?;
        Ok(Self {
            arena: source.decoded_backing()?,
            source,
            incremental: incremental_utf8_table(conn, table),
        })
    }

    /// What a query selects for `column` so [`Cells::cell`] can read it from columns `i` and
    /// `i + 1`: the rowid and the cell's type when reads are incremental, else the cell itself.
    pub(crate) fn select(&self, column: &str) -> String {
        if self.incremental {
            format!("rowid, typeof({column})")
        } else {
            format!("{column}, NULL")
        }
    }

    /// The cell `select(column)` put at column `i` of `row`. None for a cell that is not TEXT,
    /// is empty or is not UTF-8 (UTF-8 is checked when it is read).
    pub(crate) fn cell(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
    ) -> Result<Option<Cell>> {
        use rusqlite::types::ValueRef;
        let (offset, len) = if self.incremental {
            if row.get_ref(i + 1)?.as_str_or_null()? != Some("text") {
                return Ok(None);
            }
            append_cell(conn, table, column, row.get(i)?, &self.arena)?
        } else {
            let ValueRef::Text(bytes) = row.get_ref(i)? else {
                return Ok(None);
            };
            self.arena.append(|writer| Ok(writer.write_all(bytes)?))?
        };
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// The BLOB `select(column)` put at column `i` of `row`, passed through `decode` (a
    /// decompressor, say) into the arena: the decoded bytes are the cell. None for a cell that is
    /// not a BLOB or is empty; a `decode` failure is an error.
    pub(crate) fn blob(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
        decode: impl FnOnce(&mut dyn std::io::BufRead, &mut dyn std::io::Write) -> Result<()>,
    ) -> Result<Option<Cell>> {
        use rusqlite::types::ValueRef;
        let (offset, len) = if self.incremental {
            if row.get_ref(i + 1)?.as_str_or_null()? != Some("blob") {
                return Ok(None);
            }
            let blob = conn.blob_open(rusqlite::DatabaseName::Main, table, column, row.get(i)?, true)?;
            if blob.is_empty() {
                return Ok(None);
            }
            let mut blob = std::io::BufReader::with_capacity(64 * 1024, blob);
            self.arena.append(|writer| decode(&mut blob, writer))?
        } else {
            let ValueRef::Blob(mut bytes) = row.get_ref(i)? else {
                return Ok(None);
            };
            if bytes.is_empty() {
                return Ok(None);
            }
            self.arena.append(|writer| decode(&mut bytes, writer))?
        };
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// A TEXT cell holding a plain string (not JSON), copied into the arena as one JSON string so
    /// the decoder views it like any other string. None for a cell that is not TEXT, is empty or
    /// is not UTF-8.
    pub(crate) fn text(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
    ) -> Result<Option<Cell>> {
        let mut valid = false;
        let (offset, len) = self.arena.append(|writer| {
            super::text::generate_string(writer, |builder| {
                valid = self.chunks(conn, table, column, row, i, |chunk| builder.push(chunk))?;
                Ok(())
            })
        })?;
        Ok((valid && len > 2).then_some(Cell(offset, len)))
    }

    /// Visit the UTF-8 chunks of the TEXT cell `select(column)` put at column `i` of `row`
    /// without copying it anywhere. False for a cell that is not TEXT or is not UTF-8.
    pub(crate) fn chunks(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
        mut visit: impl FnMut(&str) -> Result<()>,
    ) -> Result<bool> {
        use rusqlite::types::ValueRef;
        if self.incremental {
            if row.get_ref(i + 1)?.as_str_or_null()? != Some("text") {
                return Ok(false);
            }
            return with_cell_reader(conn, table, column, row.get(i)?, |reader, _| {
                for_each_utf8(reader, &mut visit)
            });
        }
        let ValueRef::Text(bytes) = row.get_ref(i)? else {
            return Ok(false);
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return Ok(false);
        };
        visit(text)?;
        Ok(true)
    }

    /// The captured store and the arena its cells are copied into, where a decoder composes text
    /// that is checked against the store like the cells themselves.
    pub(crate) fn out(&self) -> (&crate::json::JsonSource, &std::sync::Arc<super::bytes::ByteSource>) {
        (&self.source, &self.arena)
    }

    /// The cell at `rowid` of `table.column`, TEXT or BLOB bytes as stored, for a store that
    /// locates its rows itself (by key). None when empty.
    pub(crate) fn row(&self, conn: &Connection, table: &str, column: &str, rowid: i64) -> Result<Option<Cell>> {
        let (offset, len) = append_cell(conn, table, column, rowid, &self.arena)?;
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// Bytes a format derives as a cell of their own: a BLOB in the store's own encoding rule, or
    /// a JSON string holding a JSON document. None when empty.
    pub(crate) fn append(&self, write: impl FnOnce(&mut dyn std::io::Write) -> Result<()>) -> Result<Option<Cell>> {
        let (offset, len) = self.arena.append(write)?;
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// Recheck the store and the reader's dependencies (once, when a read ends).
    pub(crate) fn check(&self) -> Result<()> {
        self.source.check()
    }

    /// Decode `cell` with `f`. None when the cell is not exactly one JSON value; every other
    /// error is a source failure.
    pub(crate) fn read<T>(
        &self,
        cell: Cell,
        f: impl for<'a> FnOnce(crate::json::JsonView<'a>) -> Result<T>,
    ) -> Result<Option<T>> {
        match self.source.read_backing_value(self.arena.clone(), cell.0, cell.1, f) {
            Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => Ok(None),
            result => result.map(Some),
        }
    }
}

/// Incremental TEXT bytes use the database's encoding. Ordinary UTF-8 rowid tables support
/// bounded reads; views, virtual/WITHOUT ROWID tables and UTF-16 stores retain their typed reader.
pub(crate) fn incremental_utf8_table(conn: &Connection, table: &str) -> bool {
    let utf8 = conn
        .query_row("PRAGMA encoding", [], |row| row.get::<_, String>(0))
        .ok()
        .as_deref()
        == Some("UTF-8");
    utf8 && conn
        .query_row(&format!("PRAGMA main.table_list('{table}')"), [], |row| {
            Ok(row.get::<_, String>(2)? == "table" && !row.get::<_, bool>(4)?)
        })
        .unwrap_or(false)
}

/// Borrow a read-only cell handle while the caller's active statement/snapshot is still alive.
/// Adapter-owned table/column identifiers only. Unlike selecting a payload column, opening this
/// handle does not ask sqlite3_step to materialize the entire TEXT/BLOB value.
pub(crate) fn with_cell_reader<T>(
    conn: &Connection,
    table: &str,
    column: &str,
    rowid: i64,
    read: impl FnOnce(&mut dyn std::io::Read, usize) -> Result<T>,
) -> Result<T> {
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, table, column, rowid, true)?;
    let len = blob.len();
    read(&mut blob, len)
}

/// Strict streaming UTF-8 acceptance, including scalars crossing native read windows. False is
/// a typed-string rejection, not a read failure; callers discard any unpublished prefix output.
pub(crate) fn for_each_utf8(reader: &mut dyn std::io::Read, mut visit: impl FnMut(&str) -> Result<()>) -> Result<bool> {
    let mut buffer = [0u8; 64 * 1024 + 3];
    let mut pending = 0;
    loop {
        let read = match reader.read(&mut buffer[pending..64 * 1024]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Ok(pending == 0);
        }
        let end = pending + read;
        match std::str::from_utf8(&buffer[..end]) {
            Ok(text) => {
                visit(text)?;
                pending = 0;
            }
            Err(error) => {
                if error.error_len().is_some() {
                    return Ok(false);
                }
                let accepted = error.valid_up_to();
                visit(std::str::from_utf8(&buffer[..accepted]).unwrap())?;
                pending = end - accepted;
                buffer.copy_within(accepted..end, 0);
            }
        }
    }
}

/// The columns present on `table` (`PRAGMA table_info`). Every SQLite-backed harness grows its
/// schema by `ALTER TABLE` across versions, so an adapter probes before it selects; an unknown
/// table yields the empty set.
pub(crate) fn columns(conn: &Connection, table: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            set.extend(rows.flatten());
        }
    }
    set
}

/// Whether the database has a table named `name`.
pub(crate) fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Every file in a store directory: name, inode, size, mtime and bytes (`-shm` included).
#[cfg(test)]
pub(crate) fn store_state(dir: &Path) -> Vec<(std::ffi::OsString, u64, u64, std::time::SystemTime, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            #[cfg(unix)]
            let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
            #[cfg(not(unix))]
            let inode = 0;
            let bytes = if metadata.is_file() {
                std::fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            (
                entry.file_name(),
                inode,
                metadata.len(),
                metadata.modified().unwrap(),
                bytes,
            )
        })
        .collect();
    files.sort();
    files
}

/// A running harness: WAL mode, `sql`'s frames not checkpointed, its connection kept open.
#[cfg(test)]
pub(crate) fn live_store(database: &Path, sql: &str) -> Connection {
    let writer = Connection::open(database).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    writer.execute_batch(sql).unwrap();
    writer
}

#[cfg(test)]
mod tests {
    use super::*;

    static SNAPSHOT_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn rollback_reader_cannot_create_wal_sidecars_after_a_mode_change() {
        let (directory, database) = fixture();
        Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(1);")
            .unwrap();
        let reader = open_ro(&database).unwrap();
        let result = reader.read(|connection| {
            // The harness switches mode after the reader's before-operation guard. Closing
            // its connection checkpoints the WAL and removes its coordination sidecars.
            let writer = Connection::open(&database)?;
            writer.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO t VALUES(2);")?;
            drop(writer);
            let before = store_state(&directory);
            let _: rusqlite::Result<i64> = connection.query_row("SELECT count(*) FROM t", [], |row| row.get(0));
            assert!(
                store_state(&directory) == before,
                "read-only connection wrote harness sidecars"
            );
            Ok(())
        });
        assert!(result.is_err(), "changed source must also fail freshness");
        drop(reader);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn fixture() -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!("cv-cold-wal-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("sessions.db");
        (directory, database)
    }

    #[test]
    fn dropping_replaced_cold_snapshot_preserves_different_directory() {
        let (directory, database) = fixture();
        std::fs::write(&database, "owned snapshot").unwrap();
        let snapshot = ColdSnapshot {
            reads: crate::source::ReadSet::default(),
            directory_identity: snapshots::snapshot_identity(&directory).unwrap(),
            directory: directory.clone(),
            database,
            copied_bytes: 0,
            _owner: None,
        };
        let saved = directory.with_extension("saved");
        std::fs::rename(&directory, &saved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let different = directory.join("different-owner");
        std::fs::write(&different, "retained").unwrap();
        drop(snapshot);
        assert_eq!(std::fs::read_to_string(&different).unwrap(), "retained");
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(saved).unwrap();
    }

    #[test]
    fn incremental_text_reads_keep_utf8_acceptance_and_native_table_constraints() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE cells(value TEXT); CREATE TABLE other(id TEXT PRIMARY KEY, value TEXT) WITHOUT ROWID; CREATE VIEW projected AS SELECT * FROM cells;")?;
        assert!(incremental_utf8_table(&conn, "cells"));
        assert!(!incremental_utf8_table(&conn, "other"));
        assert!(!incremental_utf8_table(&conn, "projected"));
        let body = format!("{}🌙{}\0end", "x".repeat(65535), "界".repeat(30000));
        conn.execute("INSERT INTO cells(value) VALUES(?1)", [&body])?;
        let decoded = with_cell_reader(&conn, "cells", "value", 1, |reader, len| {
            assert_eq!(len, body.len());
            let mut decoded = String::new();
            assert!(for_each_utf8(reader, |part| {
                decoded.push_str(part);
                Ok(())
            })?);
            Ok(decoded)
        })?;
        assert_eq!(decoded, body);
        for bytes in [b"invalid\xff".as_slice(), b"truncated\xf0\x9f".as_slice()] {
            conn.execute("INSERT INTO cells(value) VALUES(CAST(?1 AS TEXT))", [bytes])?;
            assert!(!with_cell_reader(
                &conn,
                "cells",
                "value",
                conn.last_insert_rowid(),
                |reader, _| for_each_utf8(reader, |_| Ok(()))
            )?);
        }
        let utf16 = Connection::open_in_memory()?;
        utf16.execute_batch("PRAGMA encoding='UTF-16le'; CREATE TABLE cells(value TEXT)")?;
        assert!(!incremental_utf8_table(&utf16, "cells"));
        Ok(())
    }

    /// WAL frames are read through a private snapshot (reused within an operation, removed
    /// after it) and nothing in the store directory changes, `-shm` bytes included.
    #[test]
    fn live_wal_snapshot_is_private_reused_and_writes_nothing_in_the_store() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(7);");
        let before = super::store_state(&directory);
        assert!(std::fs::metadata(sidecar(&database, "-wal")).unwrap().len() > 0);
        let first = open_ro(&database).unwrap();
        assert_eq!(
            first
                .read(|r| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            7
        );
        let second = open_ro(&database).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            first._snapshot.as_ref().unwrap(),
            second._snapshot.as_ref().unwrap()
        ));
        assert!(second.execute("INSERT INTO t VALUES(8)", []).is_err());
        let private_directory = first._snapshot.as_ref().unwrap().directory.clone();
        let scope = read_scope();
        drop(first);
        drop(second);
        for _ in 0..32 {
            let reader = open_ro(&database).unwrap();
            assert_eq!(reader._snapshot.as_ref().unwrap().directory, private_directory);
            reader
                .read(|r| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap();
        }
        assert!(private_directory.exists());
        drop(scope);
        assert!(!private_directory.exists());
        assert_eq!(super::store_state(&directory), before);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retained_backing_keeps_lease_after_its_connection_and_cache_finish() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let _writer = live_store(&database, "CREATE TABLE t(x);");
        let reader = open_ro(&database).unwrap();
        let backing = std::sync::Arc::clone(reader._snapshot.as_ref().unwrap());
        let private = backing.directory.clone();
        drop(reader); // Its cache/scope are gone, but the backing retains its own owner Arc.
        snapshots::Namespace::current().reap();
        assert!(private.exists());
        drop(backing);
        assert!(!private.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn interleaved_databases_reuse_active_snapshots_and_cleanup_operation() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let _writer = live_store(&database, "CREATE TABLE t(x);");
        let second_database = directory.join("other.db");
        let _second_writer = live_store(&second_database, "CREATE TABLE t(x);");
        let scope = read_scope();
        let first = open_ro(&database).unwrap();
        let first_backing = first._snapshot.as_ref().unwrap().directory.clone();
        let second = open_ro(&second_database).unwrap();
        let second_backing = second._snapshot.as_ref().unwrap().directory.clone();
        let again = open_ro_with_copy_check(&database, |_| panic!("active A snapshot duplicated")).unwrap();
        assert_eq!(again._snapshot.as_ref().unwrap().directory, first_backing);
        drop(first);
        drop(again);
        assert!(first_backing.exists());
        drop(second);
        assert!(second_backing.exists()); // Both small backings fit the byte budget.
        drop(scope);
        assert!(!first_backing.exists());
        assert!(!second_backing.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn idle_byte_budget_evicts_lru_and_permits_only_one_oversized_backing() {
        use std::sync::Arc;
        fn backing(bytes: u64) -> Arc<ColdSnapshot> {
            let (directory, database) = fixture();
            std::fs::File::create(&database).unwrap().set_len(bytes).unwrap();
            Arc::new(ColdSnapshot {
                reads: crate::source::ReadSet::default(),
                directory_identity: snapshots::snapshot_identity(&directory).unwrap(),
                directory,
                database,
                copied_bytes: bytes,
                _owner: None,
            })
        }
        let mut cache = SnapshotCacheState::default();
        let a = backing(100);
        let b = backing(100);
        let c = backing(100);
        cache.retain_idle(&a, 256);
        cache.retain_idle(&b, 256);
        cache.retain_idle(&a, 256);
        cache.retain_idle(&c, 256);
        assert_eq!(cache.idle_bytes, 200);
        assert_eq!(Arc::strong_count(&a), 2);
        assert_eq!(Arc::strong_count(&b), 1); // Active reader survives idle eviction.
        let b_path = b.directory.clone();
        drop(b);
        assert!(!b_path.exists());
        let oversized = backing(300);
        cache.retain_idle(&oversized, 256);
        assert_eq!(cache.idle.len(), 1);
        assert_eq!(cache.idle_bytes, 300);
        let tiny = backing(1);
        cache.retain_idle(&tiny, 256);
        assert_eq!(cache.idle.len(), 1);
        assert_eq!(cache.idle_bytes, 1);
        assert_eq!(Arc::strong_count(&oversized), 1);
    }

    #[test]
    fn cached_snapshot_rebuilds_after_source_change() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(7);");
        let scope = read_scope();
        let first = open_ro(&database).unwrap();
        let old_backing = first._snapshot.as_ref().unwrap().directory.clone();
        let old_reads = first.reads().clone();
        drop(first);
        writer.execute("UPDATE t SET x=8", []).unwrap();
        let mut copied = false;
        let second = open_ro_with_copy_check(&database, |_| {
            copied = true;
            Ok(())
        })
        .unwrap();
        assert!(copied);
        assert!(!old_backing.exists());
        assert!(!old_reads.fresh(&mut crate::source::Probe::default()));
        assert_eq!(
            second
                .read(|r| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            8
        );
        drop(second);
        drop(scope);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copy_race_rejects_source_change_and_removes_private_backing() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = live_store(&database, "CREATE TABLE t(x);");
        let mut private = None;
        let result = open_ro_with_copy_check(&database, |copied| {
            private = copied.parent().map(Path::to_path_buf);
            writer.execute("INSERT INTO t VALUES(9)", [])?;
            Ok(())
        });
        assert!(result.err().unwrap().to_string().contains("dependencies changed"));
        assert!(!private.unwrap().exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A WAL-mode store without frames (checkpointed while its harness runs, or closed) is read
    /// in place with `immutable=1`: no copy, no `-wal`/`-shm` created, nothing changed.
    #[test]
    fn frameless_wal_reads_the_original_immutably_and_writes_nothing() {
        let (directory, database) = fixture();
        let writer = live_store(
            &database,
            "CREATE TABLE t(x); INSERT INTO t VALUES(7); PRAGMA wal_checkpoint(TRUNCATE);",
        );
        assert!(sidecar(&database, "-shm").exists());
        let read = || {
            let before = super::store_state(&directory);
            let reader = open_ro_with_copy_check(&database, |_| panic!("a frameless WAL was copied")).unwrap();
            assert!(reader._snapshot.is_none());
            assert_eq!(
                reader
                    .read(|r| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                    .unwrap(),
                7
            );
            assert!(reader.execute("INSERT INTO t VALUES(8)", []).is_err());
            drop(reader);
            assert_eq!(super::store_state(&directory), before);
        };
        read();
        drop(writer);
        assert!(!sidecar(&database, "-wal").exists() && !sidecar(&database, "-shm").exists());
        read();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_shm_snapshot_preserves_wal_only_rows_and_rejects_mutation() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = Connection::open(&database).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x); PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO t VALUES(7);").unwrap();
        // Simulate a retained/crashed WAL without its coordination sidecar. The writer remains
        // alive solely to demonstrate why immutable=1 would incorrectly omit this committed row.
        std::fs::remove_file(sidecar(&database, "-shm")).unwrap();
        let reader = open_ro(&database).unwrap();
        assert_eq!(
            reader
                .read(|r| Ok(r.query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            1
        );
        assert!(!sidecar(&database, "-shm").exists());
        let immutable = Connection::open_with_flags(
            format!("file:{}?immutable=1", database.display()),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert_eq!(
            immutable
                .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(immutable);
        let failure = reader
            .read::<()>(|_| {
                writer.execute("INSERT INTO t VALUES(8)", [])?;
                anyhow::bail!("body failure")
            })
            .unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"));
        assert!(crate::json::JsonSource::capture_with_reads(&database, reader.reads()).is_err());
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn append_text_and_blob_cells_share_one_backing() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE payload(data); INSERT INTO payload VALUES('text'); INSERT INTO payload VALUES(X'626C6F62'); INSERT INTO payload VALUES(NULL);").unwrap();
        let arena = super::super::bytes::ByteSource::arena().unwrap();
        assert_eq!(append_cell(&connection, "payload", "data", 1, &arena).unwrap(), (0, 4));
        assert_eq!(append_cell(&connection, "payload", "data", 2, &arena).unwrap(), (4, 4));
        assert!(append_cell(&connection, "payload", "data", 3, &arena).is_err());
        let mut output = Vec::new();
        arena.copy_to(&mut output).unwrap();
        assert_eq!(output, b"textblob");
    }

    #[test]
    fn probes_and_read_only_open() {
        let dir = std::env::temp_dir().join(format!("cv-store-sqlite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE sessions (id TEXT, title TEXT); INSERT INTO sessions VALUES ('a', 't');")
                .unwrap();
        }
        let ro = open_ro(&db).unwrap();
        assert!(table_exists(&ro, "sessions"));
        assert!(!table_exists(&ro, "messages"));
        let cols = columns(&ro, "sessions");
        assert!(cols.contains("id") && cols.contains("title") && cols.len() == 2);
        assert!(columns(&ro, "nope").is_empty());
        // Read-only means a write is refused, not performed.
        assert!(ro.execute("INSERT INTO sessions VALUES ('b', 'u')", []).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
