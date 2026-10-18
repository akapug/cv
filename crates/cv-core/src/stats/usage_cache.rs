//! Versioned ordered projections. One operation pins a read snapshot, while a separate serial
//! writer publishes complete source rows and dependency stamps in the same transaction.
use super::{UsageBatch, UsageHit, UsageRecords};
use crate::{ir::SessionRef, source::Baseline, spill::Record};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::{
    io::{BufRead, Read, Write},
    sync::{Arc, Mutex},
};
// Bump when usage projection, response identity, model resolution or adapter interpretation changes.
const VERSION: i64 = 1;
const CONSUMER: &str = "usage-v1";
const CHUNK: usize = 64 * 1024;
const SOURCE_LIMIT: u64 = 16 * 1024 * 1024;
const TOTAL_LIMIT: i64 = 256 * 1024 * 1024;
const SOURCE_COUNT_LIMIT: i64 = 65536;
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
impl Cache {
    pub(super) fn new() -> Option<Self> {
        let writer = crate::catalog::open_db()?;
        configure(&writer)?;
        writer.execute_batch("BEGIN IMMEDIATE").ok()?;
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
             CREATE TABLE IF NOT EXISTS usage_budget(singleton INTEGER PRIMARY KEY CHECK(singleton=1),bytes INTEGER NOT NULL,sources INTEGER NOT NULL);"
        ).ok()?;
        if writer
            .query_row("SELECT singleton FROM usage_budget WHERE singleton=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .is_err()
        {
            writer
                .execute(
                    "INSERT OR IGNORE INTO usage_budget SELECT 1,COALESCE(SUM(bytes),0),COUNT(*) FROM usage_cache",
                    [],
                )
                .ok()?;
        }
        writer
            .execute_batch(
                "CREATE TRIGGER IF NOT EXISTS usage_insert AFTER INSERT ON usage_cache BEGIN
             UPDATE usage_budget SET bytes=bytes+NEW.bytes,sources=sources+1 WHERE singleton=1; END;
             CREATE TRIGGER IF NOT EXISTS usage_delete AFTER DELETE ON usage_cache BEGIN
             UPDATE usage_budget SET bytes=bytes-OLD.bytes,sources=sources-1 WHERE singleton=1; END;
             CREATE TRIGGER IF NOT EXISTS usage_update AFTER UPDATE OF bytes ON usage_cache BEGIN
             UPDATE usage_budget SET bytes=bytes+NEW.bytes-OLD.bytes WHERE singleton=1; END;",
            )
            .ok()?;
        writer.execute_batch("COMMIT").ok()?;
        let reader = crate::catalog::open_db()?;
        configure(&reader)?;
        reader.execute_batch("BEGIN").ok()?;
        Some(Self {
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
            pending: Arc::new(Mutex::new(Pending::default())),
        })
    }
    pub(super) fn load(&self, r: &SessionRef, current: &Baseline) -> Option<UsageBatch> {
        let conn = self.reader.lock().ok()?;
        let path = r.path.to_string_lossy().into_owned();
        // SQL admits bounded header bytes before rusqlite creates the owned buffer.
        let (fallback, rows, bytes): (Vec<u8>, i64, i64) = conn
            .query_row(
                "SELECT fallback,rows,bytes FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3 AND version=?4
             AND length(fallback)<=?5 AND rows>=0 AND bytes>=length(fallback) AND bytes<=?6",
                params![
                    r.harness.as_str(),
                    r.id,
                    path,
                    VERSION,
                    CHUNK as i64,
                    SOURCE_LIMIT as i64
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()?;
        if !crate::source::fresh_in(&conn, CONSUMER, r, current, &header(&fallback, rows, bytes as u64)) {
            return None;
        }
        let payload_bytes = bytes as u64 - fallback.len() as u64;
        let fallback = String::from_utf8(fallback).ok()?;
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
        if let Ok(reader) = self.reader.lock() {
            let _ = reader.execute_batch("COMMIT; BEGIN");
        }
        if let Ok(writer) = self.writer.lock() {
            if let Ok(mut pending) = self.pending.lock() {
                if writer.is_autocommit() && pending.checkpoint {
                    let _ = writer.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
                    pending.checkpoint = false;
                }
            }
        }
    }
    pub(super) fn finish(&self) {
        if let Ok(writer) = self.writer.lock() {
            if !writer.is_autocommit() && writer.execute_batch("COMMIT").is_err() {
                let _ = writer.execute_batch("ROLLBACK");
            }
            if let Ok(mut pending) = self.pending.lock() {
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
            return None;
        }
        let conn = self.writer.lock().ok()?;
        if conn.is_autocommit() {
            conn.execute_batch("BEGIN IMMEDIATE").ok()?;
        }
        conn.execute_batch("SAVEPOINT usage_source").ok()?;
        let start = (|| -> Option<()> {
            conn.execute(
                "DELETE FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3",
                params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
            )
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
            .ok()?;
            Some(())
        })();
        if start.is_none() {
            let _ = conn.execute_batch("ROLLBACK TO usage_source; RELEASE usage_source");
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
/// The output row the source baseline is published with.
fn header(fallback: &[u8], rows: i64, bytes: u64) -> Vec<u8> {
    let mut out = VERSION.to_le_bytes().to_vec();
    out.extend(rows.to_le_bytes());
    out.extend(bytes.to_le_bytes());
    out.extend(fallback);
    out
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
fn configure(conn: &Connection) -> Option<()> {
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA cache_size=-128; PRAGMA mmap_size=0;")
        .ok()
}
#[derive(Debug, thiserror::Error)]
#[error("invalid persisted usage projection")]
pub(super) struct ReadError;
pub(super) struct CachedRows {
    reader: ChunkReader,
    remaining: u64,
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
            UsageHit::read(&mut self.reader, &mut Some(SOURCE_LIMIT as usize))
                .context("reading cached usage projection")
        };
        self.failed = result.is_err();
        if self.failed {
            if let Ok(conn) = self.eviction.lock() {
                let _ = conn.execute_batch("ROLLBACK");
                let _ = evict(&conn, self.reader.harness, &self.reader.id, &self.reader.path);
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
fn evict(conn: &Connection, harness: &str, id: &str, path: &str) -> Option<()> {
    conn.execute(
        "DELETE FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3",
        params![harness, id, path],
    )
    .ok()?;
    conn.execute(
        "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        params![CONSUMER, harness, id, path],
    )
    .ok()?;
    Some(())
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
        if hit.write(self).is_err() {
            self.failed = true;
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
        if self.failed || self.flush_chunk().is_err() {
            return;
        }
        let Ok(conn) = self.conn.lock() else { return };
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
            .ok()?;
            let fallback: Vec<u8> = conn
                .query_row(
                    "SELECT fallback FROM usage_cache WHERE harness=?1 AND id=?2 AND path=?3",
                    params![self.r.harness.as_str(), self.r.id, self.r.path.to_string_lossy()],
                    |r| r.get(0),
                )
                .ok()?;
            let output = header(&fallback, self.rows, bytes);
            crate::source::publish_in(&conn, CONSUMER, &self.r, &self.source, &output).ok()?;
            if !crate::source::fresh_in(&conn, CONSUMER, &self.r, &self.source, &output) {
                return None;
            }
            loop {
                let (bytes, sources): (i64, i64) = conn
                    .query_row("SELECT bytes,sources FROM usage_budget WHERE singleton=1", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .ok()?;
                if bytes <= TOTAL_LIMIT && sources <= SOURCE_COUNT_LIMIT {
                    break;
                }
                let (h, id, path): (String, String, String) = conn
                    .query_row(
                        "SELECT harness,id,path FROM usage_cache ORDER BY updated LIMIT 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .ok()?;
                evict(&conn, &h, &id, &path)?;
            }
            conn.execute_batch("RELEASE usage_source").ok()?;
            self.committed = true;
            let mut pending = self.pending.lock().ok()?;
            pending.sources += 1;
            pending.bytes += self.bytes + self.fallback_bytes as u64;
            if pending.sources >= 64 || pending.bytes >= SOURCE_LIMIT {
                if conn.execute_batch("COMMIT").is_err() {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                *pending = Pending {
                    checkpoint: true,
                    ..Pending::default()
                };
            }
            Some(())
        })();
        let _ = result;
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        if !self.committed {
            if let Ok(conn) = self.conn.lock() {
                let _ = conn.execute_batch("ROLLBACK TO usage_source; RELEASE usage_source");
            }
        }
    }
}
impl Write for Publisher {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let len = bytes.len();
        self.bytes = self.bytes.saturating_add(len as u64);
        if self.bytes + self.fallback_bytes as u64 > SOURCE_LIMIT {
            return Err(std::io::Error::other("usage cache source budget"));
        }
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

    fn fixture(tag: &str) -> (cv_testkit::InProcess<crate::env::Scope>, SessionRef) {
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
        cache.finish();
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
        cache.finish();
        let mut failed = cache.publisher(r.clone(), before, "partial").unwrap();
        failed.bytes = SOURCE_LIMIT;
        failed.hit(&hit(1));
        failed.finish();
        cache.finish();
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
    fn valid_looking_cost_corruption_is_rejected_and_evicted() {
        let (_world, r) = fixture("usage-cache-checksum");
        let cache = Cache::new().unwrap();
        let mut writer = cache.publisher(r.clone(), source(&r, None), "final").unwrap();
        writer.hit(&hit(0));
        writer.finish();
        cache.finish();
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
