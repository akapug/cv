//! Versioned ordered projections. One operation pins a read snapshot, while a separate serial
//! writer publishes complete source rows and dependency stamps in the same transaction.
use super::{UsageBatch, UsageHit, UsageRecords};
use crate::{ir::SessionRef, source::Baseline, spill::Record};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    io::{BufRead, Read, Write},
    sync::{Arc, Mutex},
};
/// The rows' version column is the one stored-format version: a change to the usage projection,
/// response identity or model resolution bumps [`crate::source::VERSION`] like any other.
const VERSION: i64 = crate::source::VERSION as i64;
/// The `usage` consumer's record payload: the digest of the cached projection's header row.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Projected {
    header: u64,
}

impl crate::source::Payload for Projected {
    const TAG: &'static str = "usage";
    const BY_ROWS: bool = true;
}

/// The records' key, kept from 0.17 so the rows it wrote are replaced, not orphaned.
const CONSUMER: &str = "usage-v1";
const CHUNK: usize = 64 * 1024;
/// Publications commit in batches of this many projection bytes (or 64 sources): a transaction
/// size, not a cap on what is cached.
const COMMIT_BYTES: u64 = 16 * 1024 * 1024;
/// The fewest rows one pass's sweep examines ([`sweep`]); a pass over `n` sources examines `2n`.
const SWEEP_MIN: usize = 64;
type Shared = Arc<Mutex<Connection>>;
#[derive(Default)]
struct Pending {
    sources: usize,
    bytes: u64,
    checkpoint: bool,
}

pub(super) struct Cache {
    reader: Shared,
    writer: Shared,
    pending: Arc<Mutex<Pending>>,
}
fn note(error: &impl std::fmt::Display) {
    crate::env::warn_once("the usage cache", format_args!("{error:#}"));
}

impl Cache {
    /// The cache over the catalog, or `None`: a catalog that cannot open has said so at its door,
    /// and a cache whose tables cannot be set up says so once ([`crate::env::warn_once`]).
    pub(super) fn new() -> Option<Self> {
        if crate::env::frozen() {
            return None;
        }
        let (writer, reader) = (crate::catalog::open_db()?, crate::catalog::open_db()?);
        Self::over(writer, reader)
            .map_err(|e| crate::env::warn_once("the usage cache", format_args!("{e:#}")))
            .ok()
    }
    fn over(writer: Connection, reader: Connection) -> rusqlite::Result<Self> {
        configure(&writer)?;
        writer.execute_batch("BEGIN IMMEDIATE")?;
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_cache(
             harness TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,version INTEGER NOT NULL,
             fallback BLOB NOT NULL,rows INTEGER NOT NULL,bytes INTEGER NOT NULL,updated INTEGER NOT NULL,
             PRIMARY KEY(harness,id,path));
             CREATE INDEX IF NOT EXISTS usage_cache_updated ON usage_cache(updated);
             CREATE TABLE IF NOT EXISTS usage_chunks(
             harness TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,chunk INTEGER NOT NULL,payload BLOB NOT NULL,digest BLOB,
             PRIMARY KEY(harness,id,path,chunk),
             FOREIGN KEY(harness,id,path) REFERENCES usage_cache(harness,id,path) ON DELETE CASCADE);
             CREATE TABLE IF NOT EXISTS usage_sweep(singleton INTEGER PRIMARY KEY CHECK(singleton=1),next INTEGER NOT NULL);
             DROP TRIGGER IF EXISTS usage_insert;
             DROP TRIGGER IF EXISTS usage_delete;
             DROP TRIGGER IF EXISTS usage_update;
             DROP TABLE IF EXISTS usage_budget;",
        )?;
        writer.execute_batch("COMMIT")?;
        configure(&reader)?;
        reader.execute_batch("BEGIN")?;
        Ok(Self {
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
            pending: Arc::new(Mutex::new(Pending::default())),
        })
    }
    pub(super) fn load(&self, r: &SessionRef, current: &Baseline) -> Option<UsageBatch> {
        let conn = self.reader.lock().inspect_err(note).ok()?;
        let path = r.path.to_string_lossy().into_owned();
        // SQL admits bounded header bytes before rusqlite creates the owned buffer.
        let (fallback, rows, bytes): (Vec<u8>, i64, i64) = conn
            .query_row(
                "SELECT fallback,rows,bytes FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3 AND version=?4
             AND length(fallback)<=?5 AND rows>=0 AND bytes>=length(fallback)",
                params![r.harness.as_str(), r.id, path, VERSION, CHUNK as i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .inspect_err(note)
            .ok()??;
        if !crate::source::fresh_in(&conn, CONSUMER, r, current, &header(&fallback, rows, bytes as u64)) {
            return None;
        }
        let payload_bytes = bytes as u64 - fallback.len() as u64;
        let fallback = String::from_utf8(fallback).inspect_err(note).ok()?;
        Some(UsageBatch {
            hits: UsageRecords::Cached(CachedRows {
                reader: ChunkReader {
                    conn: self.reader.clone(),
                    harness: r.harness.as_str(),
                    id: r.id.clone(),
                    path,
                    next: 0,
                    bytes_left: payload_bytes,
                    buffer: Vec::new(),
                    offset: 0,
                },
                remaining: rows as u64,
                // No string in a row is longer than the projection that holds it.
                limit: usize::try_from(payload_bytes).unwrap_or(usize::MAX),
                failed: false,
                eviction: self.writer.clone(),
            }),
            fallback_model: fallback,
            publication: None,
        })
    }
    /// No cached row iterator survives a completed worker chunk. Release its read snapshot
    /// so cold ledger publication cannot pin an entire corpus worth of SQLite WAL frames.
    pub(super) fn finish_chunk(&self) {
        if let Ok(reader) = self.reader.lock().inspect_err(note) {
            if let Err(error) = reader.execute_batch("COMMIT; BEGIN") {
                crate::env::warn_once("usage cache read snapshot", error);
            }
        }
        if let Ok(writer) = self.writer.lock().inspect_err(note) {
            if let Ok(mut pending) = self.pending.lock().inspect_err(note) {
                if writer.is_autocommit() && pending.checkpoint {
                    if let Err(error) = writer.execute_batch("PRAGMA wal_checkpoint(PASSIVE)") {
                        crate::env::warn_once("usage cache checkpoint", error);
                    }
                    pending.checkpoint = false;
                }
            }
        }
    }
    /// End a pass over `visited` sources: commit what it published, then sweep ([`sweep`]).
    pub(super) fn finish(&self, visited: usize) {
        if let Ok(mut writer) = self.writer.lock().inspect_err(note) {
            if !writer.is_autocommit() {
                commit(&mut writer);
            }
            sweep(&mut writer, visited);
            if let Ok(mut pending) = self.pending.lock().inspect_err(note) {
                *pending = Pending {
                    checkpoint: true,
                    ..Pending::default()
                };
            }
        }
        self.finish_chunk();
    }
    pub(super) fn publisher(&self, r: SessionRef, source: Baseline, fallback: &str) -> Option<Publisher> {
        if fallback.len() > CHUNK {
            note(&"model metadata exceeds the source budget");
            return None;
        }
        let conn = self.writer.lock().inspect_err(note).ok()?;
        if conn.is_autocommit() {
            conn.execute_batch("BEGIN IMMEDIATE").inspect_err(note).ok()?;
        }
        conn.execute_batch("SAVEPOINT usage_source").inspect_err(note).ok()?;
        let start = (|| -> Option<()> {
            conn.execute(
                "DELETE FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3",
                params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
            )
            .inspect_err(note)
            .ok()?;
            conn.execute(
                "INSERT INTO usage_cache(harness,id,path,version,fallback,rows,bytes,updated) VALUES(?1,?2,?3,?4,?5,0,0,?6)",
                params![
                    r.harness.as_str(),
                    r.id,
                    r.path.to_string_lossy(),
                    VERSION,
                    fallback.as_bytes(),
                    chrono::Utc::now().timestamp_micros()
                ],
            )
            .inspect_err(note).ok()?;
            Some(())
        })();
        if start.is_none() {
            if let Err(error) = conn.execute_batch("ROLLBACK TO usage_source; RELEASE usage_source") {
                crate::env::warn_once("usage cache rollback", error);
            }
            return None;
        }
        Some(Publisher {
            conn: self.writer.clone(),
            r,
            source,
            buffer: Vec::with_capacity(CHUNK),
            next: 0,
            rows: 0,
            bytes: 0,
            fallback_bytes: fallback.len(),
            failed: false,
            committed: false,
            pending: self.pending.clone(),
        })
    }
}
/// The output row the source record is published with.
fn header(fallback: &[u8], rows: i64, bytes: u64) -> Projected {
    let mut out = VERSION.to_le_bytes().to_vec();
    out.extend(rows.to_le_bytes());
    out.extend(bytes.to_le_bytes());
    out.extend(fallback);
    Projected {
        header: crate::source::digest(&out),
    }
}
fn chunk_digest(harness: &str, id: &str, path: &str, index: i64, payload: &[u8]) -> Vec<u8> {
    let mut hash = crate::digest::Sha256::new();
    for value in [harness, id, path] {
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    hash.update(&index.to_le_bytes());
    hash.update(payload);
    hash.finish_hex().into_bytes()
}
fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA cache_size=-128; PRAGMA mmap_size=0;")
}
#[derive(Debug, thiserror::Error)]
#[error("invalid persisted usage projection")]
pub(super) struct ReadError;
pub(super) struct CachedRows {
    reader: ChunkReader,
    remaining: u64,
    limit: usize,
    failed: bool,
    eviction: Shared,
}
impl Iterator for CachedRows {
    type Item = Result<UsageHit>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let result = if self.remaining == 0 {
            match self.reader.fill_buf() {
                Ok([]) => return None,
                Ok(_) => Err(anyhow::anyhow!("extra cached usage bytes")),
                Err(e) => Err(e.into()),
            }
        } else {
            UsageHit::read(&mut self.reader, &mut Some(self.limit)).context("reading cached usage projection")
        };
        self.failed = result.is_err();
        if self.failed {
            if let Ok(mut conn) = self.eviction.lock().inspect_err(note) {
                // A publisher may own the surrounding transaction. Roll back only this
                // reader's eviction if either deletion fails, never the pending publication.
                let eviction = (|| -> rusqlite::Result<()> {
                    let mut savepoint = conn.savepoint()?;
                    if let Err(error) = evict(&savepoint, self.reader.harness, &self.reader.id, &self.reader.path) {
                        if let Err(rollback) = savepoint.rollback() {
                            crate::env::warn_once("usage cache eviction rollback", rollback);
                        }
                        return Err(error);
                    }
                    savepoint.commit()
                })();
                if let Err(error) = eviction {
                    crate::env::warn_once("usage cache eviction", error);
                }
            }
        } else {
            self.remaining -= 1;
        }
        Some(result.map_err(|error| error.context(ReadError)))
    }
}
struct ChunkReader {
    conn: Shared,
    harness: &'static str,
    id: String,
    path: String,
    next: i64,
    bytes_left: u64,
    buffer: Vec<u8>,
    offset: usize,
}
impl BufRead for ChunkReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.offset == self.buffer.len() && self.bytes_left > 0 {
            let conn = self
                .conn
                .lock()
                .map_err(|_| std::io::Error::other("usage cache lock"))?;
            // Reject oversized/corrupt BLOBs using SQL length before allocating their bytes.
            let (buffer,digest):(Vec<u8>,Vec<u8>)=conn.query_row(
                "SELECT payload,digest FROM usage_chunks WHERE harness=?1 AND id=?2 AND path=?3 AND chunk=?4 AND length(payload) BETWEEN 1 AND ?5 AND length(digest)=64",
                params![self.harness,self.id,self.path,self.next,CHUNK as i64],|r|Ok((r.get(0)?,r.get(1)?))
            ).map_err(std::io::Error::other)?;
            if digest != chunk_digest(self.harness, &self.id, &self.path, self.next, &buffer) {
                return Err(std::io::Error::other("cached usage checksum mismatch"));
            }
            self.buffer = buffer;
            if self.buffer.len() as u64 > self.bytes_left {
                return Err(std::io::Error::other("invalid cached usage chunk"));
            }
            self.bytes_left -= self.buffer.len() as u64;
            self.next += 1;
            self.offset = 0;
        }
        Ok(&self.buffer[self.offset..])
    }
    fn consume(&mut self, amount: usize) {
        self.offset += amount;
    }
}
impl Read for ChunkReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let bytes = self.fill_buf()?;
        let len = bytes.len().min(output.len());
        output[..len].copy_from_slice(&bytes[..len]);
        self.consume(len);
        Ok(len)
    }
}
fn evict(conn: &Connection, harness: &str, id: &str, path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3",
        params![harness, id, path],
    )?;
    conn.execute(
        "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        params![CONSUMER, harness, id, path],
    )?;
    Ok(())
}

/// Publication is durable before any maintenance starts.
fn commit(conn: &mut Connection) {
    if let Err(error) = conn.execute_batch("COMMIT") {
        crate::env::warn_once("usage cache commit", error);
        if let Err(error) = conn.execute_batch("ROLLBACK") {
            crate::env::warn_once("usage cache rollback", error);
        }
    }
}

/// The cache holds one row per source key and grows with the store, never past it: a pass
/// examines `max(SWEEP_MIN, 2 * visited)` rows after a persisted cursor and evicts only rows
/// that can never be loaded again: another projection version, an unknown harness, no source
/// record, or a source path that no longer exists. A live row is never evicted, so a warm pass
/// reloads every source it published, however large the store. A failed sweep rolls back
/// alone; the cursor stays and the next pass retries it.
fn sweep(conn: &mut Connection, visited: usize) {
    let budget = visited.saturating_mul(2).max(SWEEP_MIN);
    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let next: i64 = tx
            .query_row("SELECT next FROM usage_sweep WHERE singleton=1", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        let rows: Vec<(i64, String, String, String, bool)> = {
            let mut stmt = tx.prepare(
                "SELECT u.rowid,u.harness,u.id,u.path,
                 u.version=?3 AND EXISTS(SELECT 1 FROM source_dependencies d
                   WHERE d.consumer=?4 AND d.harness=u.harness AND d.id=u.id AND d.path=u.path)
                 FROM usage_cache u WHERE u.rowid>=?1 ORDER BY u.rowid LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![next, budget as i64, VERSION, CONSUMER], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // A short page reached the end: the next pass starts over from the first row.
        let next = match rows.last() {
            Some((rowid, ..)) if rows.len() == budget => rowid + 1,
            _ => 0,
        };
        for (_, harness, id, path, current) in &rows {
            let gone =
                || std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if !current || crate::ir::Harness::parse(harness).is_none() || gone() {
                evict(&tx, harness, id, path)?;
            }
        }
        tx.execute(
            "INSERT INTO usage_sweep(singleton,next) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET next=?1",
            [next],
        )?;
        tx.commit()
    })();
    if let Err(error) = result {
        crate::env::warn_once("usage cache eviction", error);
    }
}

pub(super) struct Publisher {
    conn: Shared,
    r: SessionRef,
    source: Baseline,
    buffer: Vec<u8>,
    next: i64,
    rows: i64,
    bytes: u64,
    fallback_bytes: usize,
    failed: bool,
    committed: bool,
    pending: Arc<Mutex<Pending>>,
}
impl Publisher {
    pub(super) fn hit(&mut self, hit: &UsageHit) {
        if self.failed {
            return;
        }
        if hit.write(self).inspect_err(note).is_err() {
            self.failed = true;
            return;
        }
        self.rows += 1;
    }
    fn flush_chunk(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("usage cache lock"))?;
        conn.execute(
            "INSERT INTO usage_chunks(harness,id,path,chunk,payload,digest) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                self.r.harness.as_str(),
                self.r.id,
                self.r.path.to_string_lossy(),
                self.next,
                self.buffer,
                chunk_digest(
                    self.r.harness.as_str(),
                    &self.r.id,
                    &self.r.path.to_string_lossy(),
                    self.next,
                    &self.buffer
                )
            ],
        )
        .map_err(std::io::Error::other)?;
        self.next += 1;
        self.buffer.clear();
        Ok(())
    }
    pub(super) fn finish(mut self) {
        if self.failed || self.flush_chunk().inspect_err(note).is_err() {
            return;
        }
        let Ok(mut conn) = self.conn.lock().inspect_err(note) else {
            return;
        };
        let result = (|| -> Option<()> {
            let bytes = self.bytes + self.fallback_bytes as u64;
            conn.execute(
                "UPDATE usage_cache SET rows=?4,bytes=?5 WHERE harness=?1 AND id=?2 AND path=?3",
                params![
                    self.r.harness.as_str(),
                    self.r.id,
                    self.r.path.to_string_lossy(),
                    self.rows,
                    bytes as i64,
                ],
            )
            .inspect_err(note)
            .ok()?;
            let fallback: Vec<u8> = conn
                .query_row(
                    "SELECT fallback FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3",
                    params![self.r.harness.as_str(), self.r.id, self.r.path.to_string_lossy()],
                    |r| r.get(0),
                )
                .inspect_err(note)
                .ok()?;
            let output = header(&fallback, self.rows, bytes);
            crate::source::publish_in(&conn, CONSUMER, &self.r, &self.source, &output, &mut Default::default())
                .inspect_err(note)
                .ok()?;
            if !crate::source::fresh_in(&conn, CONSUMER, &self.r, &self.source, &output) {
                return None;
            }
            conn.execute_batch("RELEASE usage_source").inspect_err(note).ok()?;
            self.committed = true;
            let mut pending = self.pending.lock().inspect_err(note).ok()?;
            pending.sources += 1;
            pending.bytes += self.bytes + self.fallback_bytes as u64;
            if pending.sources >= 64 || pending.bytes >= COMMIT_BYTES {
                commit(&mut conn);
                *pending = Pending {
                    checkpoint: true,
                    ..Pending::default()
                };
            }
            Some(())
        })();
        if result.is_none() {
            crate::env::warn_once(
                "usage cache publication",
                "source or database changed before publication",
            );
        }
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        if !self.committed {
            if let Ok(conn) = self.conn.lock().inspect_err(note) {
                if let Err(error) = conn.execute_batch("ROLLBACK TO usage_source; RELEASE usage_source") {
                    crate::env::warn_once("usage cache rollback", error);
                }
            }
        }
    }
}
impl Write for Publisher {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let len = bytes.len();
        self.bytes = self.bytes.saturating_add(len as u64);
        while !bytes.is_empty() {
            let n = bytes.len().min(CHUNK - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buffer.len() == CHUNK {
                self.flush_chunk()?;
            }
        }
        Ok(len)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_chunk()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Harness, Usage};
    fn load(r: &SessionRef, current: &Baseline) -> Option<UsageBatch> {
        Cache::new()?.load(r, current)
    }
    fn open() -> Option<Connection> {
        crate::catalog::open_db()
    }

    fn fixture(tag: &str) -> (cv_testkit::InProcess<crate::env::Isolated>, SessionRef) {
        let world = crate::test_world(tag);
        let path = world.base.join("source.jsonl");
        std::fs::write(&path, "source").unwrap();
        let r = SessionRef::new("source", Harness::Claude, path);
        (world, r)
    }
    fn source(r: &SessionRef, dependency: Option<&std::path::Path>) -> Baseline {
        let source = crate::source::Probe::default().snapshot(r);
        let mut reads = crate::source::ReadSet::default();
        reads.record(&r.path);
        if let Some(path) = dependency {
            reads.record(path);
        }
        source.with_reads(&reads)
    }
    fn hit(i: usize) -> UsageHit {
        UsageHit {
            key: Some(format!("claude:{i}")),
            harness: Harness::Claude,
            model: if i == 100 { "m".repeat(CHUNK + 7) } else { String::new() },
            usage: Usage {
                input_tokens: Some(1),
                cost_usd: Some(f64::from_bits(0x3fb999999999999b)),
                ..Usage::default()
            },
        }
    }
    #[test]
    fn ordered_chunks_preserve_raw_models_cost_bits_and_dependency_absence() {
        let (world, r) = fixture("usage-cache-chunks");
        let absent = world.base.join("late-dependency");
        let cache = Cache::new().unwrap();
        let mut writer = cache
            .publisher(r.clone(), source(&r, Some(&absent)), "final-model")
            .unwrap();
        for i in 0..9000 {
            writer.hit(&hit(i));
        }
        writer.finish();
        cache.finish(1);
        let batch = load(&r, &crate::source::Probe::default().snapshot(&r)).unwrap();
        assert_eq!(batch.fallback_model, "final-model");
        let mut count = 0;
        for (i, row) in batch.hits.rows().unwrap().enumerate() {
            let row = row.unwrap();
            let expected = hit(i);
            assert_eq!(row.key, expected.key);
            assert_eq!(row.model, expected.model);
            assert_eq!(
                row.usage.cost_usd.unwrap().to_bits(),
                expected.usage.cost_usd.unwrap().to_bits()
            );
            count += 1;
        }
        assert_eq!(count, 9000);
        std::fs::write(&absent, "created").unwrap();
        assert!(load(&r, &crate::source::Probe::default().snapshot(&r)).is_none());
    }
    #[test]
    fn failed_publication_rolls_back_and_changed_sources_or_versions_miss() {
        let (_world, r) = fixture("usage-cache-atomic");
        let before = source(&r, None);
        let cache = Cache::new().unwrap();
        let mut writer = cache.publisher(r.clone(), before.clone(), "first").unwrap();
        writer.hit(&hit(0));
        writer.finish();
        cache.finish(1);
        open()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_chunk BEFORE INSERT ON usage_chunks
                 BEGIN SELECT RAISE(ABORT, 'injected chunk failure'); END;",
            )
            .unwrap();
        let mut failed = cache.publisher(r.clone(), before, "partial").unwrap();
        failed.hit(&hit(1));
        failed.finish();
        assert!(!crate::env::warn_once("the usage cache", "already reported"));
        cache.finish(1);
        open().unwrap().execute_batch("DROP TRIGGER fail_chunk").unwrap();
        let batch = load(&r, &crate::source::Probe::default().snapshot(&r)).unwrap();
        assert_eq!(batch.fallback_model, "first");
        assert_eq!(
            batch.hits.rows().unwrap().next().unwrap().unwrap().key,
            Some("claude:0".into())
        );
        open().unwrap().execute("UPDATE usage_cache SET version=0", []).unwrap();
        assert!(load(&r, &crate::source::Probe::default().snapshot(&r)).is_none());
        open().unwrap().execute("UPDATE usage_cache SET version=1", []).unwrap();
        std::fs::write(&r.path, "source changed").unwrap();
        assert!(load(&r, &crate::source::Probe::default().snapshot(&r)).is_none());
    }
    #[test]
    fn corrupt_read_during_publication_preserves_the_writer_transaction() {
        for fail_eviction in [false, true] {
            let (world, r) = fixture("usage-cache-reader-transaction");
            let cache = Cache::new().unwrap();
            let mut writer = cache.publisher(r.clone(), source(&r, None), "old").unwrap();
            writer.hit(&hit(0));
            writer.finish();
            cache.finish(1);
            let conn = open().unwrap();
            conn.execute("UPDATE usage_chunks SET digest=zeroblob(64)", []).unwrap();
            if fail_eviction {
                conn.execute_batch(
                    "CREATE TRIGGER fail_eviction BEFORE DELETE ON source_dependencies
                     BEGIN SELECT RAISE(ABORT, 'injected eviction failure'); END;",
                )
                .unwrap();
            }
            let path = world.base.join("other.jsonl");
            std::fs::write(&path, "other source").unwrap();
            let other = SessionRef::new("other", Harness::Claude, path);
            let mut writer = cache.publisher(other.clone(), source(&other, None), "new").unwrap();
            writer.hit(&hit(1));
            writer.flush_chunk().unwrap();
            let current = source(&r, None);
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let batch = cache.load(&r, &current).unwrap();
                        let error = batch.hits.rows().unwrap().next().unwrap().err().unwrap();
                        assert!(error.downcast_ref::<ReadError>().is_some());
                    })
                    .join()
                    .unwrap();
            });
            assert!(!cache.writer.lock().unwrap().is_autocommit());
            writer.finish();
            cache.finish(1);
            let batch = load(&other, &source(&other, None)).unwrap();
            assert_eq!(batch.fallback_model, "new");
            let mut rows = batch.hits.rows().unwrap();
            assert_eq!(rows.next().unwrap().unwrap().key, hit(1).key);
            assert!(rows.next().is_none());
            // The two eviction deletes are atomic even inside the writer's transaction.
            assert_eq!(load(&r, &current).is_some(), fail_eviction);
        }
    }
    fn publish(cache: &Cache, r: &SessionRef, i: usize) {
        let mut writer = cache.publisher(r.clone(), source(r, None), "model").unwrap();
        writer.hit(&hit(i));
        writer.finish();
    }
    fn rows_cached() -> i64 {
        open()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM usage_cache", [], |r| r.get(0))
            .unwrap()
    }
    #[test]
    fn sweep_evicts_only_rows_that_can_never_load_and_keeps_every_live_source() {
        let (world, r) = fixture("usage-cache-sweep");
        let cache = Cache::new().unwrap();
        let live: Vec<SessionRef> = (0..3 * SWEEP_MIN)
            .map(|i| {
                let path = world.base.join(format!("live-{i}.jsonl"));
                std::fs::write(&path, "live source").unwrap();
                SessionRef::new(format!("live-{i}"), Harness::Claude, path)
            })
            .collect();
        publish(&cache, &r, 0);
        for (i, live) in live.iter().enumerate() {
            publish(&cache, live, i + 1);
        }
        let old = SessionRef::new("old-version", Harness::Claude, world.base.join("live-0.jsonl"));
        publish(&cache, &old, 0);
        cache.finish(0);
        // One pass's sweep examines a bounded page after its cursor; a pass over every source
        // examines the whole table. Only the deleted source and the old version go.
        std::fs::remove_file(&r.path).unwrap();
        open()
            .unwrap()
            .execute("UPDATE usage_cache SET version=0 WHERE id='old-version'", [])
            .unwrap();
        assert_eq!(rows_cached(), live.len() as i64 + 2);
        let cursor = || -> i64 {
            open()
                .unwrap()
                .query_row("SELECT next FROM usage_sweep", [], |r| r.get(0))
                .unwrap()
        };
        let at = cursor();
        cache.finish(0);
        assert_eq!(
            cursor(),
            at + SWEEP_MIN as i64,
            "a pass over nothing examines one bounded page"
        );
        // A pass over every source examines (twice) the whole table, wrapping at its end.
        cache.finish(live.len() + 2);
        cache.finish(live.len() + 2);
        assert_eq!(rows_cached(), live.len() as i64);
        for live in &live {
            assert!(
                load(live, &source(live, None)).is_some(),
                "a live row survives every sweep"
            );
        }
    }
    #[test]
    fn failed_sweep_keeps_committed_publications_and_retries() {
        let (world, r) = fixture("usage-cache-eviction-failure");
        let cache = Cache::new().unwrap();
        publish(&cache, &r, 0);
        cache.finish(1);
        std::fs::remove_file(&r.path).unwrap();
        let conn = open().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_eviction BEFORE DELETE ON source_dependencies
             BEGIN SELECT RAISE(ABORT, 'injected eviction failure'); END;",
        )
        .unwrap();
        let path = world.base.join("new.jsonl");
        std::fs::write(&path, "new source").unwrap();
        let new = SessionRef::new("new", Harness::Claude, path);
        publish(&cache, &new, 1);
        cache.finish(1);
        assert!(cache.writer.lock().unwrap().is_autocommit());
        assert!(!crate::env::warn_once("usage cache eviction", "already reported"));
        // The publication committed before the failed sweep; the sweep rolled back as a unit.
        assert!(load(&new, &source(&new, None)).is_some());
        assert_eq!(rows_cached(), 2);
        conn.execute_batch("DROP TRIGGER fail_eviction").unwrap();
        cache.finish(1);
        assert_eq!(rows_cached(), 1);
        assert!(load(&new, &source(&new, None)).is_some());
    }
    #[test]
    fn valid_looking_cost_corruption_is_rejected_and_evicted() {
        let (_world, r) = fixture("usage-cache-checksum");
        let cache = Cache::new().unwrap();
        let mut writer = cache.publisher(r.clone(), source(&r, None), "final").unwrap();
        writer.hit(&hit(0));
        writer.finish();
        cache.finish(1);
        open()
            .unwrap()
            .execute(
                "UPDATE usage_chunks SET payload=CAST(substr(payload,1,length(payload)-1)||x'40' AS BLOB)",
                [],
            )
            .unwrap();
        let batch = load(&r, &crate::source::Probe::default().snapshot(&r)).unwrap();
        let error = batch.hits.rows().unwrap().next().unwrap().err().unwrap();
        assert!(error.downcast_ref::<ReadError>().is_some());
        assert!(load(&r, &crate::source::Probe::default().snapshot(&r)).is_none());
    }
}
