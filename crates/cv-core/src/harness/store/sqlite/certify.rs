//! [`Rows`] certificates of a shared store's sessions ([`crate::harness::Adapter::rows`]), folded
//! from the rows each session's read consults. A certificate is a metadata guard, like a resume
//! mark's samples: rowids, counts, keys, small columns and the byte lengths of large ones, never a
//! decoded body. A length is read from the record header (`octet_length` does not load the value),
//! so a large cell's overflow pages are never read.

use crate::source::Rows;
use anyhow::Result;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::collections::HashMap;

/// Fold one SQLite value into a certificate digest: its type, its length, its bytes.
pub(crate) fn fold_value(digest: u64, value: ValueRef<'_>) -> u64 {
    let integer;
    let (tag, bytes): (u8, &[u8]) = match value {
        ValueRef::Null => (0, &[]),
        ValueRef::Integer(i) => {
            integer = i.to_le_bytes();
            (1, &integer)
        }
        ValueRef::Real(f) => {
            integer = f.to_bits().to_le_bytes();
            (2, &integer)
        }
        ValueRef::Text(t) => (3, t),
        ValueRef::Blob(b) => (4, b),
    };
    let digest = Rows::fold(digest, &[tag]);
    let digest = Rows::fold(digest, &(bytes.len() as u64).to_le_bytes());
    Rows::fold(digest, bytes)
}

/// The certificates of one store, built up by the queries an adapter's read consults.
#[derive(Default)]
pub(crate) struct Certificates(pub(crate) HashMap<String, Rows>);

impl Certificates {
    /// The certificate of every session `sql` names in its first column (a TEXT id; other rows
    /// are skipped), created empty, with every further column folded into its digest in the
    /// query's order: a session's own small rows (its metadata row, a side table).
    pub(crate) fn digest(&mut self, conn: &Connection, sql: &str) -> Result<()> {
        self.each(conn, sql, false)
    }

    /// [`Self::digest`] of one row per message: the second column is the row's rowid, counted
    /// and raising the session's high-water mark. A query names every column whose change
    /// changes the read (ordering keys, flags, the lengths of its bodies).
    pub(crate) fn rows(&mut self, conn: &Connection, sql: &str) -> Result<()> {
        self.each(conn, sql, true)
    }

    fn each(&mut self, conn: &Connection, sql: &str, counted: bool) -> Result<()> {
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let ValueRef::Text(id) = row.get_ref(0)? else { continue };
            let Ok(id) = std::str::from_utf8(id) else { continue };
            let rows = match self.0.get_mut(id) {
                Some(rows) => rows,
                None => self.0.entry(id.to_owned()).or_insert(Rows {
                    count: 0,
                    high: 0,
                    digest: Rows::SEED,
                }),
            };
            if counted {
                rows.count += 1;
                rows.high = rows.high.max(row.get::<_, Option<i64>>(1)?.unwrap_or(0));
            }
            for i in 1..columns {
                rows.digest = fold_value(rows.digest, row.get_ref(i)?);
            }
        }
        Ok(())
    }

    /// Fold a fact `id`'s read derives from elsewhere into its digest (no-op for an uncertified
    /// `id`).
    pub(crate) fn fold(&mut self, id: &str, bytes: &[u8]) {
        if let Some(rows) = self.0.get_mut(id) {
            rows.digest = Rows::fold(Rows::fold(rows.digest, &(bytes.len() as u64).to_le_bytes()), bytes);
        }
    }

    /// Fold `with`'s certificate (or its absence) into `id`'s digest: a read of `id` that
    /// consults another session's rows.
    pub(crate) fn fold_from(&mut self, id: &str, with: Option<Rows>) {
        let with = with.map_or([0; 3], |w| [w.count, w.high as u64, w.digest]);
        let bytes: Vec<u8> = with.iter().flat_map(|part| part.to_le_bytes()).collect();
        self.fold(id, &bytes);
    }
}

/// A shared store's certificates, tested the way every adapter's are: each session `read`
/// consults is certified, deterministically; after `write` puts one row into `leaf`, `leaf`'s
/// certificate moves and every other session's holds, and every session whose certificate held
/// reads exactly as before.
#[cfg(test)]
pub(crate) fn assert_rows_move_alone(
    adapter: &dyn crate::harness::Adapter,
    db: &std::path::Path,
    leaf: &str,
    write: impl FnOnce(&Connection),
) {
    use crate::stream::Demand;
    let certify = || adapter.rows(db).expect("the store certifies its sessions");
    let before = certify();
    assert_eq!(certify(), before, "{}: a certificate is deterministic", db.display());
    let refs = adapter.discover().unwrap().refs;
    let refs: Vec<_> = refs.into_iter().filter(|r| r.path == db).collect();
    assert!(refs.len() > 1, "{}: a shared store", db.display());
    assert!(
        refs.iter().any(|r| r.id == leaf),
        "{leaf} is a session of {}",
        db.display()
    );
    let key = |r: &crate::SessionRef| adapter.rows_key(r).to_owned();
    for r in &refs {
        assert!(before.contains_key(&key(r)), "{} is certified", r.id);
    }
    let read = |r: &crate::SessionRef| {
        let s = crate::stream::collect_with(adapter, r, &Demand::RICH).unwrap();
        serde_json::to_string(&s).unwrap()
    };
    let reads: Vec<String> = refs.iter().map(read).collect();
    write(&Connection::open(db).unwrap());
    let after = certify();
    let leaf_key = key(refs.iter().find(|r| r.id == leaf).unwrap());
    assert_ne!(after[&leaf_key], before[&leaf_key], "{leaf}: the written session moves");
    for (r, old) in refs.iter().zip(&reads) {
        if key(r) == leaf_key {
            continue;
        }
        assert_eq!(after[&key(r)], before[&key(r)], "{}: another session's write", r.id);
        assert_eq!(read(r), *old, "{}: certified unchanged, read changed", r.id);
    }
}
