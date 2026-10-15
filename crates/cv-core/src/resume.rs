//! Byte marks: where a pass over a grown append-only transcript picks back up.
//!
//! A pass that streams through [`stream::stream_counted`](crate::stream::stream_counted) with checkpoints
//! takes [`Checkpoint`]s as it goes; its last one, stored here as a **mark** per transcript file,
//! lets the next pass over the same file (grown by an append) read only what came after it. A
//! mark passes its guards when the file's device/inode identity matches, it is at least `offset`
//! bytes long, the sampled head and bytes immediately before `offset` hash as they did, and it was
//! written by this very parser ([`version`]: the cv release, the affordance view, the adapter's
//! state format and the consumer's). Anything else means a whole pass, which writes a new mark.
//!
//! These samples do not establish that the entire prefix is unchanged. Resuming assumes the
//! adapter's immutable-prefix append contract; an in-place edit outside the samples (past the
//! first 4 KiB and before the last 4 KiB under the mark) followed by growth escapes detection, and
//! the docs and counts of the edited bytes stay as they were read. Proving arbitrary prefix edits
//! absent requires reading that prefix again: hashing the whole prefix on every resume was
//! measured and rejected (docs/design/PERF.md, "Resume marks"). Consumers must separately reject
//! non-growth rewrites; the FTS sweep compares the current size with the size of its previously
//! indexed source.
//!
//! The marks live in the catalog db beside the counts and events they continue (`resume_marks`);
//! like the rest of the catalog it is a cache, best-effort throughout.

use crate::harness::Adapter;
use crate::ir::SessionRef;
use crate::stream::Checkpoint;
use std::path::Path;

/// How many bytes the prefix hash reads at the head of the file and again just before the mark.
const HASH_SPAN: u64 = 4096;

/// A stored mark minus its state: version, position and sampled-byte guard for append reuse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MarkHead {
    pub offset: u64,
    pub hash: u64,
    pub version: String,
    /// Device/inode on Unix. Legacy marks without identity are rejected once.
    pub identity: Option<(u64, u64)>,
}

impl MarkHead {
    /// The head of a mark for `cp`, taken on `path` by a pass at `version`. `None` when the file
    /// can no longer be read up to the checkpoint, or stable file identity is unavailable.
    /// Device/inode guards are currently implemented on Unix; other platforms read whole.
    pub fn of(path: &Path, cp: &Checkpoint, version: &str) -> Option<MarkHead> {
        Self::of_file(&std::fs::File::open(path).ok()?, cp, version)
    }

    /// [`Self::of`] on an open handle: the identity and samples of the file that handle names,
    /// whatever its path names now. A pass that opens its source before reading takes its mark
    /// this way, so a file renamed over the path mid-pass is never paired with this pass's
    /// checkpoint (its next pass sees another inode and reads whole).
    pub fn of_file(file: &std::fs::File, cp: &Checkpoint, version: &str) -> Option<MarkHead> {
        let (identity, hash) = prefix_guard_file(file, cp.offset)?;
        identity?; // Without a stable file identity, prefer a whole read.
        Some(MarkHead {
            offset: cp.offset,
            hash,
            version: version.to_string(),
            identity,
        })
    }

    /// Whether the file identity, version, minimum length and sampled bytes match. This does
    /// not establish that the whole old prefix is unchanged or that the file grew since its read.
    pub fn holds(&self, path: &Path, size: u64, version: &str) -> bool {
        self.version == version
            && size >= self.offset
            && self.identity.is_some()
            && prefix_guard(path, self.offset) == Some((self.identity, self.hash))
    }
}

/// The version a mark records and must match: the cv release, the affordance view, the adapter's
/// checkpoint format and `consumer` (the caller's own state format). `None`: the adapter takes no
/// checkpoints.
pub fn version(adapter: &dyn Adapter, consumer: &str) -> Option<String> {
    Some(format!(
        "{}/view{}/{}{}/{consumer}",
        env!("CARGO_PKG_VERSION"),
        crate::affordance::VIEW_VERSION,
        adapter.harness().as_str(),
        adapter.resume_format()?
    ))
}

/// FNV-1a (64-bit) over the first [`HASH_SPAN`] bytes of `path` and the [`HASH_SPAN`] bytes
/// before `offset` (overlapping on a short file). `None` when the file is shorter than `offset`.
pub fn prefix_hash(path: &Path, offset: u64) -> Option<u64> {
    prefix_guard(path, offset).map(|(_, hash)| hash)
}

fn prefix_guard(path: &Path, offset: u64) -> Option<(Option<(u64, u64)>, u64)> {
    prefix_guard_file(&std::fs::File::open(path).ok()?, offset)
}

fn prefix_guard_file(file: &std::fs::File, offset: u64) -> Option<(Option<(u64, u64)>, u64)> {
    use crate::harness::store::jsonl::ranges::read_exact_at;
    let metadata = file.metadata().ok()?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let identity = None;
    if metadata.len() < offset {
        return None;
    }
    let mut head = vec![0u8; offset.min(HASH_SPAN) as usize];
    read_exact_at(file, &mut head, 0).ok()?;
    let from = offset.saturating_sub(HASH_SPAN);
    let mut tail = vec![0u8; (offset - from) as usize];
    read_exact_at(file, &mut tail, from).ok()?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in head.iter().chain(&tail).chain(&offset.to_le_bytes()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    Some((identity, h))
}

/// A session and its new mark (`None`: it has none any more), for [`update`].
pub type MarkUpdate = (SessionRef, Option<(MarkHead, Checkpoint)>);

#[cfg(feature = "sqlite")]
mod imp {
    use super::*;
    use rusqlite::{params, Connection};
    use std::collections::HashMap;

    fn open() -> Option<Connection> {
        let conn = crate::catalog::open_db()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS resume_marks(
                 harness TEXT NOT NULL,
                 id      TEXT NOT NULL,
                 path    TEXT NOT NULL,
                 offset  INTEGER NOT NULL,
                 hash    INTEGER NOT NULL,
                 version TEXT NOT NULL,
                 state   TEXT NOT NULL,
                 PRIMARY KEY(harness, id, path)
             );",
        )
        .ok()?;
        let has_identity: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('resume_marks') WHERE name='identity')",
                [],
                |row| row.get(0),
            )
            .ok()?;
        if !has_identity {
            let _ = conn.execute_batch("ALTER TABLE resume_marks ADD COLUMN identity TEXT;");
        }
        Some(conn)
    }

    fn key(r: &SessionRef) -> (String, String, String) {
        (
            r.harness.as_str().to_string(),
            r.id.clone(),
            r.path.to_string_lossy().into_owned(),
        )
    }

    /// Every mark's head, loaded with one query for a sweep (the states, which can be tens of KB
    /// each, are read only for the sessions that resume: [`state`]).
    pub struct MarkTable {
        rows: HashMap<(String, String, String), MarkHead>,
    }

    impl MarkTable {
        pub fn load() -> MarkTable {
            let load = || -> Option<HashMap<(String, String, String), MarkHead>> {
                let conn = open()?;
                let mut stmt = conn
                    .prepare("SELECT harness, id, path, offset, hash, version, identity FROM resume_marks")
                    .ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            (row.get(0)?, row.get(1)?, row.get(2)?),
                            MarkHead {
                                offset: row.get::<_, i64>(3)? as u64,
                                hash: row.get::<_, i64>(4)? as u64,
                                version: row.get(5)?,
                                identity: row
                                    .get::<_, Option<String>>(6)?
                                    .and_then(|text| serde_json::from_str(&text).ok()),
                            },
                        ))
                    })
                    .ok()?;
                Some(rows.flatten().collect())
            };
            MarkTable {
                rows: load().unwrap_or_default(),
            }
        }

        pub fn get(&self, r: &SessionRef) -> Option<&MarkHead> {
            self.rows.get(&key(r))
        }
    }

    /// The checkpoint `r`'s mark holds, when it is still the one `head` describes.
    pub fn state(r: &SessionRef, head: &MarkHead) -> Option<Checkpoint> {
        let conn = open()?;
        let (h, i, p) = key(r);
        let (offset, hash, version, text, identity): (i64, i64, String, String, Option<String>) = conn
            .query_row(
                "SELECT offset, hash, version, state, identity FROM resume_marks WHERE harness=?1 AND id=?2 AND path=?3",
                params![h, i, p],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .ok()?;
        let same = MarkHead {
            offset: offset as u64,
            hash: hash as u64,
            version,
            identity: identity.and_then(|text| serde_json::from_str(&text).ok()),
        } == *head;
        let cp: Checkpoint = serde_json::from_str(&text).ok()?;
        (same && cp.offset == head.offset).then_some(cp)
    }

    /// Bring each session's mark up to date in one transaction: the new mark replaces the old,
    /// `None` drops it (the next pass over that file reads it whole).
    pub fn update(marks: &[MarkUpdate]) {
        if marks.is_empty() {
            return;
        }
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        {
            let Ok(mut put) = tx.prepare(
                "INSERT OR REPLACE INTO resume_marks(harness,id,path,offset,hash,version,state,identity)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            ) else {
                return;
            };
            let Ok(mut drop) = tx.prepare("DELETE FROM resume_marks WHERE harness=?1 AND id=?2 AND path=?3") else {
                return;
            };
            for (r, mark) in marks {
                let (h, i, p) = key(r);
                let _ = match mark {
                    Some((head, cp)) => {
                        let Ok(text) = serde_json::to_string(cp) else { continue };
                        put.execute(params![
                            h,
                            i,
                            p,
                            head.offset as i64,
                            head.hash as i64,
                            head.version,
                            text,
                            head.identity.and_then(|id| serde_json::to_string(&id).ok())
                        ])
                    }
                    None => drop.execute(params![h, i, p]),
                };
            }
        }
        let _ = tx.commit();
    }
}

#[cfg(feature = "sqlite")]
pub use imp::{state, update, MarkTable};

/// Without the catalog there are no marks: every pass reads the whole file.
#[cfg(not(feature = "sqlite"))]
pub struct MarkTable;
#[cfg(not(feature = "sqlite"))]
impl MarkTable {
    pub fn load() -> MarkTable {
        MarkTable
    }
    pub fn get(&self, _r: &SessionRef) -> Option<&MarkHead> {
        None
    }
}
#[cfg(not(feature = "sqlite"))]
pub fn state(_r: &SessionRef, _head: &MarkHead) -> Option<Checkpoint> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub fn update(_marks: &[MarkUpdate]) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sampling an open transcript must not move its reader or any cloned handle.
    #[test]
    #[cfg(unix)]
    fn sampling_preserves_the_open_readers_position_and_hash() {
        use std::io::{Read, Seek, SeekFrom};
        let world = cv_testkit::World::new("mark-position");
        let path = world.base.join("transcript");
        let body: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut reader = file.try_clone().unwrap();
        for offset in [0u64, 100, 4096, 10_000, body.len() as u64] {
            file.seek(SeekFrom::Start(123)).unwrap();
            let mark = MarkHead::of_file(&file, &Checkpoint::new(offset), "v1").unwrap();
            let end = offset as usize;
            let expected = body[..end.min(HASH_SPAN as usize)]
                .iter()
                .chain(&body[end.saturating_sub(HASH_SPAN as usize)..end])
                .chain(&offset.to_le_bytes())
                .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
                });
            assert_eq!(mark.hash, expected);
            assert_eq!(file.stream_position().unwrap(), 123);
            let mut next = [0; 8];
            reader.read_exact(&mut next).unwrap();
            assert_eq!(next, body[123..131]);
        }
        file.seek(SeekFrom::Start(123)).unwrap();
        assert!(MarkHead::of_file(&file, &Checkpoint::new(body.len() as u64 + 1), "v1").is_none());
        assert_eq!(file.stream_position().unwrap(), 123);
    }

    /// A mark permits growth; sampled-byte edits, truncation and another version break it.
    #[test]
    #[cfg(unix)]
    fn a_mark_checks_samples_length_and_version_across_an_append() {
        let dir = std::env::temp_dir().join(format!("cv-mark-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let line = |i: usize| format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "x".repeat(i % 97));
        let body: String = (0..400).map(line).collect();
        std::fs::write(&path, &body).unwrap();
        let offset = body[..body.len() / 2].rfind('\n').unwrap() as u64 + 1;
        let head = MarkHead::of(&path, &Checkpoint::new(offset), "v1").unwrap();
        let size = |p: &Path| std::fs::metadata(p).unwrap().len();

        assert!(head.holds(&path, size(&path), "v1"));
        assert!(!head.holds(&path, size(&path), "v2"), "another version");

        let legacy = MarkHead {
            identity: None,
            ..head.clone()
        };
        assert!(
            !legacy.holds(&path, size(&path), "v1"),
            "legacy identity must refresh once"
        );
        let grown = format!("{body}{}", line(401));
        std::fs::write(&path, &grown).unwrap();
        assert!(head.holds(&path, size(&path), "v1"), "an append keeps it");
        let replacement = dir.join("replacement.jsonl");
        std::fs::write(&replacement, &grown).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(prefix_hash(&path, offset), Some(head.hash));
        assert!(
            !head.holds(&path, size(&path), "v1"),
            "replacement identity differs even with identical samples"
        );

        let head = MarkHead::of(&path, &Checkpoint::new(offset), "v1").unwrap();
        assert!(head.holds(&path, size(&path), "v1"));

        // A mark taken on a handle opened before a rename-over describes the file read, so it
        // does not hold for what the path names now; reopening the path would have paired them.
        let read = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, &grown).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let of_read = MarkHead::of_file(&read, &Checkpoint::new(offset), "v1").unwrap();
        assert_eq!(of_read.hash, head.hash);
        assert!(!of_read.holds(&path, size(&path), "v1"));
        let head = MarkHead::of(&path, &Checkpoint::new(offset), "v1").unwrap();

        // One byte changed just before the mark, then one in the head: both break it.
        for at in [offset as usize - 10, 7] {
            let mut b = grown.clone().into_bytes();
            b[at] = b'y';
            std::fs::write(&path, &b).unwrap();
            assert!(!head.holds(&path, size(&path), "v1"), "byte {at} changed");
        }
        std::fs::write(&path, &grown.as_bytes()[..offset as usize - 1]).unwrap();
        assert!(!head.holds(&path, size(&path), "v1"), "truncated before the mark");
        std::fs::remove_dir_all(&dir).ok();
    }
}
