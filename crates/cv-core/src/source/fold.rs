//! Rebuildable decoded checkpoints of canonical source records, demanded by exact copy key.
//!
//! The canonical row remains the authority. A derived consumer in the same native table holds
//! its decoded envelope (raw stamp strings, not JSON to scan again). Three transactional
//! invalidation triggers cover inserts, updates and deletes, including older cv writers. No
//! catalog shape/version change or corpus rebuild: absent/read-only caches use canonical rows.
//! A plan retains each demanded answer; a new plan/CLI process consults current SQL authority.

use super::*;
use crate::stage_work::{add, enter, Metric, Stage};
use rusqlite::{params, Connection, OptionalExtension};

const PREFIX: &str = "baseline-fold-v1/";
const MAGIC: &[u8] = b"CVBF\x01";
const LOOKUP: &str = "SELECT coalesce(f.record,s.record),f.record IS NOT NULL
 FROM source_dependencies s LEFT JOIN source_dependencies f
 ON f.consumer=?5 AND f.harness=s.harness AND f.id=s.id AND f.path=s.path
 AND (SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name IN
     ('cv_baseline_fold_insert_v1','cv_baseline_fold_delete_v1','cv_baseline_fold_update_v1'))=3
 WHERE s.consumer=?1 AND s.harness=?2 AND s.id=?3 AND s.path=?4";
const TRIGGERS: &str = "
CREATE TRIGGER IF NOT EXISTS cv_baseline_fold_insert_v1 AFTER INSERT ON source_dependencies
WHEN substr(NEW.consumer,1,17) != 'baseline-fold-v1/' BEGIN
 DELETE FROM source_dependencies WHERE consumer='baseline-fold-v1/'||NEW.consumer
 AND harness=NEW.harness AND id=NEW.id AND path=NEW.path;
END;
CREATE TRIGGER IF NOT EXISTS cv_baseline_fold_delete_v1 AFTER DELETE ON source_dependencies
WHEN substr(OLD.consumer,1,17) != 'baseline-fold-v1/' BEGIN
 DELETE FROM source_dependencies WHERE consumer='baseline-fold-v1/'||OLD.consumer
 AND harness=OLD.harness AND id=OLD.id AND path=OLD.path;
END;
CREATE TRIGGER IF NOT EXISTS cv_baseline_fold_update_v1 AFTER UPDATE ON source_dependencies
WHEN substr(OLD.consumer,1,17) != 'baseline-fold-v1/' BEGIN
 DELETE FROM source_dependencies WHERE consumer='baseline-fold-v1/'||OLD.consumer
 AND harness=OLD.harness AND id=OLD.id AND path=OLD.path;
 DELETE FROM source_dependencies WHERE consumer='baseline-fold-v1/'||NEW.consumer
 AND harness=NEW.harness AND id=NEW.id AND path=NEW.path;
END;";

fn equipped(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name IN
         ('cv_baseline_fold_insert_v1','cv_baseline_fold_delete_v1','cv_baseline_fold_update_v1')",
        [],
        |row| row.get::<_, usize>(0),
    )
    .ok()
        == Some(3)
}

fn stage<T: Payload>() -> Stage {
    match T::TAG {
        "counts" => Stage::BaselineCounts,
        "events" => Stage::BaselineEvents,
        "fts" => Stage::BaselineFts,
        _ => Stage::BaselineOther,
    }
}

pub(super) struct Reader {
    conn: Option<Connection>,
    attempted: bool,
    consumer: String,
    checkpoint: String,
    folds: bool,
    work: Stage,
    pending: crate::spill::Ordered<Pending>,
}

impl Reader {
    pub(super) fn new(consumer: &str) -> Self {
        Self {
            conn: None,
            attempted: false,
            consumer: consumer.into(),
            checkpoint: format!("{PREFIX}{consumer}"),
            folds: false,
            work: Stage::BaselineOther,
            pending: Default::default(),
        }
    }

    pub(super) fn with_connection(consumer: &str, conn: Connection) -> Self {
        Self {
            conn: Some(conn),
            ..Self::new(consumer)
        }
    }

    fn open(&mut self) -> Option<&Connection> {
        if !self.attempted {
            self.attempted = true;
            self.conn = self.conn.take().or_else(super::open);
            if let Some(conn) = &mut self.conn {
                // This is an optional read accelerator: cache writes must never wait for a
                // canonical publisher's writer lock. Failed writes simply miss next time.
                let _ = conn.busy_timeout(std::time::Duration::ZERO);
                // DDL and admission are atomic. Missing invalidators make every retained
                // fold untrustworthy; discard them before installing all three together.
                // Equipped and frozen/read-only catalogs never acquire a DDL writer lock.
                if !equipped(conn) && !crate::env::frozen() {
                    let mut install = || -> rusqlite::Result<()> {
                        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                        if !equipped(&tx) {
                            tx.execute(
                                "DELETE FROM source_dependencies WHERE consumer GLOB 'baseline-fold-v1/*'",
                                [],
                            )?;
                            tx.execute_batch(TRIGGERS)?;
                        }
                        tx.commit()
                    };
                    let _ = install();
                }
                self.folds = !self.consumer.starts_with(PREFIX) && equipped(conn);
            }
        }
        self.conn.as_ref()
    }

    /// Outer None is an unavailable catalog. Inner None is a missing, malformed or old record.
    pub(super) fn read<T: Payload>(&mut self, r: &SessionRef) -> Option<Option<Record<T>>> {
        self.work = stage::<T>();
        let _work = enter(self.work);
        add(Metric::Calls, 1);
        self.open()?;
        let conn = self.conn.as_ref()?;
        let (h, id, path) = key(r);
        let fetch = || -> rusqlite::Result<Option<(Vec<u8>, bool)>> {
            if self.folds {
                // Coalesce is lazy: a checkpoint hit never fetches the canonical record's
                // overflow pages. Both rows belong to this one SQLite statement snapshot.
                conn.prepare_cached(LOOKUP)?
                    .query_row(params![self.consumer, h, id, path, self.checkpoint], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .optional()
            } else {
                canonical(conn, &self.consumer, r).map(|row| row.map(|bytes| (bytes, false)))
            }
        };
        let Some((mut bytes, cached)) = fetch().ok()? else {
            return Some(None);
        };
        if cached {
            add(Metric::CheckpointInputBytes, bytes.len() as u64);
            if let Some(record) = decode(&bytes) {
                return Some(Some(record));
            }
            // Corrupt/unknown checkpoints are never authority. Read and replace just this
            // canonical row, not the consumer's whole history.
            bytes = match canonical(conn, &self.consumer, r).ok()? {
                Some(bytes) => bytes,
                None => return Some(None),
            };
        }
        add(Metric::Rows, 1);
        add(Metric::InputBytes, bytes.len() as u64);
        let Some(record) = Record::decode(&bytes) else {
            return Some(None);
        };
        add(Metric::DecodedRows, 1);
        if self.folds && !crate::env::frozen() {
            // Native ordered staging bounds cold-start retention and batches durability into
            // one transaction per table, not one fsync per discovered session.
            let _ = self.pending.push(Pending {
                key: (h, id, path),
                canonical: bytes,
                checkpoint: encode(&record),
            });
        }
        Some(Some(record))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.pending.len() == 0 || crate::env::frozen() {
            return;
        }
        let _work = enter(self.work);
        let Some(conn) = self.conn.as_mut() else { return };
        let Ok(rows) = std::mem::take(&mut self.pending).rows() else {
            return;
        };
        let flush = || -> anyhow::Result<()> {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // A lost invalidator makes queued snapshots inadmissible too.
            if !equipped(&tx) {
                return Ok(());
            }
            let mut output_bytes = 0;
            {
                let mut put = tx.prepare_cached(
                    "INSERT OR REPLACE INTO source_dependencies(consumer,harness,id,path,record)
                     SELECT ?5,harness,id,path,?6 FROM source_dependencies
                     WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4 AND record=?7",
                )?;
                for row in rows {
                    let Pending {
                        key: (h, id, path),
                        canonical,
                        checkpoint,
                    } = row?;
                    // A concurrent publication wins: bind the decode to the exact canonical
                    // bytes, under the writer transaction, before publishing a checkpoint.
                    if put.execute(params![
                        self.consumer,
                        h,
                        id,
                        path,
                        self.checkpoint,
                        checkpoint,
                        canonical
                    ])? == 1
                    {
                        output_bytes += checkpoint.len() as u64;
                    }
                }
            }
            tx.commit()?;
            add(Metric::CheckpointOutputBytes, output_bytes);
            Ok(())
        };
        let _ = flush(); // Optional acceleration never changes the canonical read's result.
    }
}

struct Pending {
    key: Key,
    canonical: Vec<u8>,
    checkpoint: Vec<u8>,
}

impl crate::spill::Record for Pending {
    type ReadState = ();
    fn heap_bytes(&self) -> usize {
        self.key.0.capacity()
            + self.key.1.capacity()
            + self.key.2.capacity()
            + self.canonical.capacity()
            + self.checkpoint.capacity()
    }
    fn write(&self, writer: &mut impl std::io::Write) -> anyhow::Result<()> {
        for bytes in [
            self.key.0.as_bytes(),
            self.key.1.as_bytes(),
            self.key.2.as_bytes(),
            &self.canonical,
            &self.checkpoint,
        ] {
            writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
            writer.write_all(bytes)?;
        }
        Ok(())
    }
    fn read(reader: &mut impl std::io::BufRead, _: &mut ()) -> anyhow::Result<Self> {
        let mut bytes = || -> anyhow::Result<Vec<u8>> {
            let mut len = [0; 8];
            reader.read_exact(&mut len)?;
            let mut bytes = vec![0; usize::try_from(u64::from_le_bytes(len))?];
            reader.read_exact(&mut bytes)?;
            Ok(bytes)
        };
        Ok(Self {
            key: (
                String::from_utf8(bytes()?)?,
                String::from_utf8(bytes()?)?,
                String::from_utf8(bytes()?)?,
            ),
            canonical: bytes()?,
            checkpoint: bytes()?,
        })
    }
}

fn canonical(conn: &Connection, consumer: &str, r: &SessionRef) -> rusqlite::Result<Option<Vec<u8>>> {
    let (h, id, path) = key(r);
    conn.prepare_cached(
        "SELECT record FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
    )?
    .query_row(params![consumer, h, id, path], |row| row.get(0))
    .optional()
}

// A length-framed decoded envelope, not a second authority or a new transcript format. The
// checksum rejects damaged derived bytes before constructing unchecked raw stamp text. Only
// payload JSON is parsed: the large stamp JSON is demanded by its existing admission methods.
fn encode<T: Payload>(record: &Record<T>) -> Vec<u8> {
    fn text(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(record.version as u64).to_le_bytes());
    text(&mut out, T::TAG.as_bytes());
    out.push(u8::from(record.baseline.primary.is_some()));
    if let Some(primary) = record.baseline.primary {
        out.extend_from_slice(&primary.to_bytes());
    }
    out.push(u8::from(record.baseline.rows.is_some()));
    if let Some(rows) = record.baseline.rows {
        for word in [rows.count, rows.high as u64, rows.digest] {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    out.push(u8::from(record.baseline.stamp.is_some()));
    if let Some(stamp) = &record.baseline.stamp {
        text(&mut out, stamp.entries.get().as_bytes());
        out.push(u8::from(stamp.reads.is_some()));
        if let Some(reads) = &stamp.reads {
            text(&mut out, reads.get().as_bytes());
        }
    }
    text(
        &mut out,
        &serde_json::to_vec(&record.payload).expect("a record serializes"),
    );
    let mut framed = Vec::with_capacity(MAGIC.len() + 8 + out.len());
    framed.extend_from_slice(MAGIC);
    framed.extend_from_slice(&digest(&out).to_le_bytes());
    framed.extend_from_slice(&out);
    framed
}

fn decode<T: Payload>(bytes: &[u8]) -> Option<Record<T>> {
    struct Input<'a>(&'a [u8]);
    impl<'a> Input<'a> {
        fn take(&mut self, len: usize) -> Option<&'a [u8]> {
            let (head, tail) = self.0.split_at_checked(len)?;
            self.0 = tail;
            Some(head)
        }
        fn word(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn flag(&mut self) -> Option<bool> {
            match self.take(1)?[0] {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            }
        }
        fn bytes(&mut self) -> Option<&'a [u8]> {
            let len = usize::try_from(self.word()?).ok()?;
            self.take(len)
        }
        fn text(&mut self) -> Option<String> {
            std::str::from_utf8(self.bytes()?).ok().map(str::to_owned)
        }
    }
    let mut input = Input(bytes.strip_prefix(MAGIC)?);
    let checksum = input.word()?;
    if digest(input.0) != checksum || input.word()? != u64::from(VERSION) || input.bytes()? != T::TAG.as_bytes() {
        return None;
    }
    let primary = if input.flag()? {
        Some(FileId::from_bytes(input.take(40)?)?)
    } else {
        None
    };
    let rows = if input.flag()? {
        Some(Rows {
            count: input.word()?,
            high: input.word()? as i64,
            digest: input.word()?,
        })
    } else {
        None
    };
    let stamp = if input.flag()? {
        Some(Stamp {
            entries: RawStamp::Folded(input.text()?.into_boxed_str()),
            reads: if input.flag()? {
                Some(RawStamp::Folded(input.text()?.into_boxed_str()))
            } else {
                None
            },
        })
    } else {
        None
    };
    let payload = serde_json::from_slice(input.bytes()?).ok()?;
    input.0.is_empty().then_some(Record {
        version: VERSION,
        baseline: Baseline { primary, stamp, rows },
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Count(u64);
    impl Payload for Count {
        const TAG: &'static str = "counts";
    }

    fn reference(id: &str) -> SessionRef {
        SessionRef::new(id, Harness::Claude, PathBuf::from(format!("/fixture/{id}")))
    }

    fn record(value: u64) -> Record<Count> {
        Record::new(
            Baseline {
                primary: Some(FileId {
                    len: value,
                    ..FileId::default()
                }),
                stamp: Stamp::new(&[], Some(&[])),
                rows: Some(Rows {
                    count: value,
                    high: value as i64,
                    digest: value,
                }),
            },
            Count(value),
        )
    }

    fn put(conn: &Connection, consumer: &str, r: &SessionRef, record: &[u8]) {
        let (h, id, path) = key(r);
        conn.execute(
            "INSERT OR REPLACE INTO source_dependencies VALUES(?1,?2,?3,?4,?5)",
            params![consumer, h, id, path, record],
        )
        .unwrap();
    }

    fn read(consumer: &str, r: &SessionRef) -> Option<Record<Count>> {
        let mut reader = Reader::new(consumer);
        reader.read(r).unwrap()
    }

    fn metric(operation: &crate::stage_work::Operation, name: &str) -> u64 {
        let report = operation.snapshot().unwrap();
        let stage = report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["stage"] == "baseline_counts")
            .unwrap();
        stage["work"].as_array().unwrap().iter().find(|m| m[0] == name).unwrap()[1]
            .as_u64()
            .unwrap()
    }

    #[test]
    fn checkpoint_codec_preserves_canonical_spelling_and_rejects_damage() {
        for value in [0, 42, u64::MAX] {
            let original = record(value);
            let bytes = encode(&original);
            assert_eq!(decode::<Count>(&bytes).unwrap().encode(), original.encode());
            for at in [0, MAGIC.len(), bytes.len() - 1] {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1;
                assert!(decode::<Count>(&damaged).is_none());
            }
            for end in 0..bytes.len() {
                assert!(decode::<Count>(&bytes[..end]).is_none());
            }
        }
        for baseline in [
            Baseline {
                primary: None,
                stamp: None,
                rows: None,
            },
            Baseline {
                primary: None,
                stamp: Stamp::new(&[], None),
                rows: None,
            },
        ] {
            let original = Record::new(baseline, Count(3));
            assert_eq!(decode::<Count>(&encode(&original)).unwrap().encode(), original.encode());
        }
    }

    #[test]
    fn checkpoint_delta_tracks_old_writers_rollback_exact_copies_and_corruption() {
        let world = crate::test_world("baseline-fold-delta");
        world.scope.set("CV_INDEX_STAGE_WORK", "1");
        let conn = super::super::open().unwrap();
        let r = reference("one");
        put(&conn, "counts", &r, &record(1).encode());
        let op = crate::stage_work::Operation::from_env(None);
        assert_eq!(read("counts", &r).unwrap().payload, Count(1));
        assert_eq!(metric(&op, "decoded_rows"), 1);
        // Fresh reader/connection, no in-process memoization: the persisted fold serves it.
        assert_eq!(read("counts", &r).unwrap().payload, Count(1));
        assert_eq!(metric(&op, "decoded_rows"), 1);
        assert!(metric(&op, "checkpoint_input_bytes") > 0);
        assert!(read("events", &r).is_none());
        for other in [
            SessionRef {
                id: "other".into(),
                ..r.clone()
            },
            SessionRef {
                harness: Harness::Codex,
                ..r.clone()
            },
            SessionRef {
                path: "/other".into(),
                ..r.clone()
            },
        ] {
            assert!(read("counts", &other).is_none());
        }
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        put(&conn, "counts", &r, &record(2).encode());
        conn.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(read("counts", &r).unwrap().payload, Count(1));
        assert_eq!(metric(&op, "decoded_rows"), 1, "rollback retained its matching fold");
        // Old binaries issue ordinary SQL: triggers must invalidate without their cooperation.
        conn.execute(
            "UPDATE source_dependencies SET record=?1 WHERE consumer='counts'",
            [record(2).encode()],
        )
        .unwrap();
        assert_eq!(read("counts", &r).unwrap().payload, Count(2));
        put(&conn, "counts", &r, &record(3).encode());
        assert_eq!(read("counts", &r).unwrap().payload, Count(3));
        put(&conn, &format!("{PREFIX}counts"), &r, b"damaged checkpoint");
        assert_eq!(read("counts", &r).unwrap().payload, Count(3));
        assert_eq!(metric(&op, "decoded_rows"), 4);
        conn.execute("DELETE FROM source_dependencies WHERE consumer='counts'", [])
            .unwrap();
        assert!(read("counts", &r).is_none());
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM source_dependencies WHERE consumer LIKE 'baseline-fold%'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        put(&conn, "counts", &r, &record(4).encode());
        assert_eq!(read("counts", &r).unwrap().payload, Count(4));
        // A plan retains exactly what it demanded even if another publication follows it.
        let mut plan = Table::<Count>::demanded("counts");
        assert!(plan.demand(&r));
        put(&conn, "counts", &r, &record(5).encode());
        assert!(plan.demand(&r));
        assert_eq!(plan.get(&r).unwrap().payload, Count(4));
        assert_eq!(read("counts", &r).unwrap().payload, Count(5));
        op.finish(true);
    }

    #[test]
    fn checkpoint_missing_invalidators_and_frozen_reads_never_trust_stale_folds() {
        let world = crate::test_world("baseline-fold-frozen");
        world.scope.set("CV_INDEX_STAGE_WORK", "1");
        let conn = super::super::open().unwrap();
        let r = reference("one");
        put(&conn, "counts", &r, &record(1).encode());
        assert_eq!(read("counts", &r).unwrap().payload, Count(1));
        let mut retained = Reader::new("counts");
        retained.open().unwrap();
        conn.execute_batch("DROP TRIGGER cv_baseline_fold_update_v1;").unwrap();
        conn.execute(
            "UPDATE source_dependencies SET record=?1 WHERE consumer='counts'",
            [record(2).encode()],
        )
        .unwrap();
        assert_eq!(
            retained.read::<Count>(&r).unwrap().unwrap().payload,
            Count(2),
            "even an open reader must stop trusting a lost invalidator"
        );
        drop(retained);
        // Reopening repairs missing invalidators and discards all potentially stale folds.
        assert_eq!(read("counts", &r).unwrap().payload, Count(2));
        let cold = reference("cold");
        put(&conn, "counts", &cold, &record(8).encode());
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        drop(conn);
        world.scope.set("CV_FROZEN", "1");
        let before = std::fs::read(world.cv_home.join("catalog.db")).unwrap();
        let op = crate::stage_work::Operation::from_env(None);
        assert_eq!(read("counts", &r).unwrap().payload, Count(2));
        assert_eq!(metric(&op, "decoded_rows"), 0);
        assert_eq!(read("counts", &cold).unwrap().payload, Count(8));
        assert_eq!(
            metric(&op, "decoded_rows"),
            1,
            "frozen cold misses use canonical rows without installing folds"
        );
        assert_eq!(metric(&op, "checkpoint_output_bytes"), 0);
        assert_eq!(std::fs::read(world.cv_home.join("catalog.db")).unwrap(), before);
        op.finish(true);
    }

    #[test]
    fn checkpoint_lookup_uses_exact_indexes_not_an_archived_baseline_scan() {
        let _world = crate::test_world("baseline-fold-plan");
        let conn = super::super::open().unwrap();
        let r = reference("one");
        put(&conn, "counts", &r, &record(1).encode());
        assert_eq!(read("counts", &r).unwrap().payload, Count(1));
        let mut explain = conn.prepare(&format!("EXPLAIN QUERY PLAN {LOOKUP}")).unwrap();
        let plan: Vec<String> = explain
            .query_map(
                params!["counts", "claude", "one", "/fixture/one", format!("{PREFIX}counts")],
                |row| row.get(3),
            )
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter().any(|line| line.starts_with("SEARCH s USING INDEX")),
            "{plan:?}"
        );
        assert!(
            plan.iter().any(|line| line.starts_with("SEARCH f USING INDEX")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|line| line.starts_with("SCAN s ") || line.starts_with("SCAN f ")),
            "{plan:?}"
        );
        let mut statement = conn.prepare(LOOKUP).unwrap();
        let (_, cached): (Vec<u8>, bool) = statement
            .query_row(
                params!["counts", "claude", "one", "/fixture/one", format!("{PREFIX}counts")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(cached, "query-plan proof must exercise a real persisted fold");
    }

    #[test]
    fn checkpoint_batch_does_not_wait_for_writers_or_publish_a_raced_decode() {
        let _world = crate::test_world("baseline-fold-race");
        let conn = super::super::open().unwrap();
        let r = reference("one");
        put(&conn, "counts", &r, &record(1).encode());
        let mut reader = Reader::new("counts");
        assert_eq!(reader.read::<Count>(&r).unwrap().unwrap().payload, Count(1));
        assert!(reader.pending.len() > 0);
        put(&conn, "counts", &r, &record(2).encode());
        drop(reader);
        assert!(
            canonical(&conn, &format!("{PREFIX}counts"), &r).unwrap().is_none(),
            "raced decode must not publish"
        );
        assert_eq!(read("counts", &r).unwrap().payload, Count(2));
        conn.execute(
            "DELETE FROM source_dependencies WHERE consumer=?1",
            [format!("{PREFIX}counts")],
        )
        .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        conn.execute(
            "UPDATE source_dependencies SET record=?1 WHERE consumer='counts'",
            [record(3).encode()],
        )
        .unwrap();
        let start = std::time::Instant::now();
        assert_eq!(
            read("counts", &r).unwrap().payload,
            Count(2),
            "reader sees last committed canonical state"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "optional checkpoint waited for writer"
        );
        conn.execute_batch("COMMIT;").unwrap();
        assert_eq!(read("counts", &r).unwrap().payload, Count(3));
    }

    #[test]
    fn checkpoint_survives_an_actual_process_restart() {
        let world = crate::test_world("baseline-fold-process");
        let conn = super::super::open().unwrap();
        let r = reference("persisted");
        put(&conn, "counts", &r, &record(7).encode());
        assert_eq!(read("counts", &r).unwrap().payload, Count(7));
        drop(conn);
        let result = world
            .command(std::env::current_exe().unwrap())
            .args(["--exact", "source::fold::tests::checkpoint_child", "--nocapture"])
            .env("CV_BASELINE_FOLD_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("persisted checkpoint hit"));
    }

    #[test]
    fn checkpoint_child() {
        if std::env::var_os("CV_BASELINE_FOLD_CHILD").is_none() {
            return;
        }
        let scope = crate::env::Env::from_process().enter();
        scope.set("CV_INDEX_STAGE_WORK", "1");
        let op = crate::stage_work::Operation::from_env(None);
        assert_eq!(read("counts", &reference("persisted")).unwrap().payload, Count(7));
        assert_eq!(metric(&op, "decoded_rows"), 0);
        assert!(metric(&op, "checkpoint_input_bytes") > 0);
        println!("persisted checkpoint hit");
        op.finish(true);
    }

    fn growth(archived: usize, active: usize) {
        let world = crate::test_world("baseline-fold-growth");
        world.scope.set("CV_INDEX_STAGE_WORK", "1");
        let mut conn = super::super::open().unwrap();
        let bytes = record(1).encode();
        let tx = conn.transaction().unwrap();
        {
            let mut put = tx
                .prepare("INSERT INTO source_dependencies VALUES('counts','claude',?1,?2,?3)")
                .unwrap();
            for n in 0..archived {
                put.execute(params![format!("archive{n:08}"), format!("/archive/{n:08}"), bytes])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        let refs: Vec<_> = (0..active).map(|n| reference(&format!("active{n}"))).collect();
        // Active records carry a nontrivial conditional-read inventory, not empty stamps.
        let dependencies: Vec<_> = (0..32)
            .map(|n| Entry {
                path: format!("/fixture/outputs/{n:04}/{}", "x".repeat(96)).into(),
                state: Some(FileId::default()),
                prefix: None,
                grows: None,
                resumes: None,
            })
            .collect();
        let active_record = |value| {
            let mut current = record(value);
            current.baseline.stamp = Stamp::new(&[], Some(&dependencies));
            current
        };
        let active_bytes = active_record(1).encode();
        let tx = conn.transaction().unwrap();
        for r in &refs {
            put(&tx, "counts", r, &active_bytes);
        }
        tx.commit().unwrap();
        // Warm the decoded fold in one bounded, batched reader, then start fresh readers.
        let mut warm = Table::<Count>::demanded("counts");
        for r in &refs {
            assert!(warm.demand(r));
        }
        drop(warm);
        let idle = Table::<Count>::demanded("counts");
        assert!(idle.rows.is_empty());
        drop(idle); // no demand means no connection, probes or checkpoint writes
        for changed in [0, 8] {
            for r in refs.iter().take(changed) {
                put(&conn, "counts", r, &active_record(2).encode());
            }
            let op = crate::stage_work::Operation::from_env(None);
            let start = std::time::Instant::now();
            let mut table = Table::<Count>::demanded("counts");
            for (n, r) in refs.iter().enumerate() {
                assert!(table.demand(r));
                assert_eq!(table.get(r).unwrap().payload, Count(if n < changed { 2 } else { 1 }));
            }
            assert_eq!(metric(&op, "decoded_rows"), changed as u64);
            assert_eq!(metric(&op, "rows"), changed as u64);
            assert_eq!(metric(&op, "input_bytes"), changed as u64 * active_bytes.len() as u64);
            drop(table); // Include checkpoint publication, not merely the reads, in cost.
            println!("baseline-fold-growth archived={archived} active={active} changed={changed} canonical_rows={} canonical_bytes={} checkpoint_bytes={} cpu_ns={} wall_us={}",
                metric(&op, "rows"), metric(&op, "input_bytes"), metric(&op, "checkpoint_input_bytes"), op.snapshot().unwrap()["process_cpu_ns"], start.elapsed().as_micros());
            op.finish(true);
        }
        // Deliberate old full-table path: prove counters see the unchanged archived input.
        let op = crate::stage_work::Operation::from_env(None);
        let old = Table::<Count>::load("counts");
        assert_eq!(old.rows.len(), archived + refs.len());
        assert_eq!(metric(&op, "decoded_rows"), (archived + refs.len()) as u64);
        println!(
            "baseline-fold-negative-control archived={archived} canonical_rows={}",
            metric(&op, "rows")
        );
        op.finish(true);
    }

    #[test]
    fn checkpoint_changed_work_is_independent_of_archived_growth() {
        for archived in [256, 2560] {
            growth(archived, 64);
        }
    }

    #[test]
    #[ignore = "realistic 250k/2.5m-row cost qualification; run explicitly on a Fab node"]
    fn checkpoint_realistic_today_and_tenfold_growth() {
        for archived in [250_000, 2_500_000] {
            growth(archived, 8400);
        }
    }
}
