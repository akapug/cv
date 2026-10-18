//! One explicitly owned catalog connection for a read/discovery operation.
use super::{Publication, SessionRef};
use crate::Harness;

/// No transaction spans filesystem scans. Separate operations reopen the current catalog path;
/// this owner has no process-global connection or schema cache.
pub(crate) struct Reader {
    #[cfg(feature = "sqlite")]
    conn: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
    #[cfg(all(test, feature = "sqlite"))]
    counts_lookups: std::cell::Cell<usize>,
    #[cfg(all(test, feature = "sqlite"))]
    counts_rows: std::cell::Cell<usize>,
}

impl Reader {
    pub(crate) fn open() -> Option<Self> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::open_db().map(|conn| Self {
                conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
                #[cfg(test)]
                counts_lookups: std::cell::Cell::new(0),
                #[cfg(test)]
                counts_rows: std::cell::Cell::new(0),
            })
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    /// Share this operation's initialized connection with scoped discovery workers.
    pub(crate) fn discovery_scope(&self) -> crate::discover_cache::read::Scope {
        #[cfg(feature = "sqlite")]
        {
            crate::discover_cache::read::Context::from_connection(self.conn.clone()).enter()
        }
        #[cfg(not(feature = "sqlite"))]
        {
            crate::discover_cache::read::Context::current().enter()
        }
    }

    /// Demand one count record and its provenance in a single statement snapshot. Separate
    /// candidates may observe different commits; no fleet snapshot or transaction spans body
    /// reads/publication. Missing, stale, malformed or unreadable cache records are cache misses.
    pub(crate) fn counts_with_probe(
        &self,
        r: &SessionRef,
        source: &crate::source::Baseline,
        probe: &mut crate::source::Probe,
    ) -> Option<crate::affordance::Counts> {
        #[cfg(feature = "sqlite")]
        {
            #[cfg(test)]
            self.counts_lookups.set(self.counts_lookups.get() + 1);
            let (counts, record) = {
                let conn = self.conn.lock().ok()?;
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT a.counts,d.record
                     FROM affordance_counts AS a JOIN source_dependencies AS d
                     ON d.consumer='counts' AND d.harness=a.harness AND d.id=a.id AND d.path=a.path
                     WHERE a.harness=?1 AND a.id=?2 AND a.path=?3",
                    )
                    .ok()?;
                stmt.query_row(
                    rusqlite::params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
                    |row| {
                        let record = (row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?);
                        #[cfg(test)]
                        self.counts_rows.set(self.counts_rows.get() + 1);
                        Ok(record)
                    },
                )
                .ok()?
            }; // Reset the SQL statement before filesystem provenance checks or body reads.
            crate::source::Published::decode(&record)?
                .of(counts.as_bytes())?
                .fresh(source, probe)
                .then_some(())?;
            super::imp::counts_of(&counts)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = (r, source, probe);
            None
        }
    }

    #[cfg(all(test, feature = "sqlite"))]
    pub(crate) fn counts_work(&self) -> (usize, usize) {
        (self.counts_lookups.get(), self.counts_rows.get())
    }

    pub(crate) fn last_full_sync(&self) -> Option<i64> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::last_full_sync_in(&self.conn.lock().unwrap())
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    /// Whether this is the first full discovery since a newer cv upgraded the catalog.
    pub(crate) fn take_rediscovery_notice(&self) -> bool {
        #[cfg(feature = "sqlite")]
        {
            super::imp::take_rediscovery_notice_in(&self.conn.lock().unwrap())
        }
        #[cfg(not(feature = "sqlite"))]
        {
            false
        }
    }

    pub(crate) fn probe_stale(&self) -> Option<Vec<Harness>> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::probe_stale_in(&self.conn.lock().unwrap())
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    pub(crate) fn all_sessions(&self) -> Option<Vec<SessionRef>> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::all_sessions_in(&self.conn.lock().unwrap())
        }
        #[cfg(not(feature = "sqlite"))]
        {
            None
        }
    }

    /// Search-row enrichment needs exactly one ID, never its prefix siblings. Preserve the
    /// existing first exact catalog copy while reusing this operation's initialized connection.
    pub(crate) fn exact_session(&self, id: &str, harness: Option<Harness>, overrides: [bool; 2]) -> Option<SessionRef> {
        #[cfg(feature = "sqlite")]
        {
            let conn = self.conn.lock().ok()?;
            super::imp::exact_session_in(&conn, id, harness, overrides)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = (id, harness, overrides);
            None
        }
    }

    pub(crate) fn primary_sessions(&self) -> Vec<SessionRef> {
        #[cfg(feature = "sqlite")]
        {
            super::imp::primary_refs_in(&self.conn.lock().unwrap(), None)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            Vec::new()
        }
    }

    pub(crate) fn capture_primary_reads(&self, a: &dyn crate::Adapter) -> super::PrimaryReads {
        #[cfg(feature = "sqlite")]
        {
            let refs = super::imp::primary_refs_in(&self.conn.lock().unwrap(), Some(a.harness()));
            super::capture_primary_reads_from(a, &refs)
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = a;
            Default::default()
        }
    }

    pub(crate) fn replace_harness(
        &mut self,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(std::path::PathBuf, super::Watched)],
        before: &super::PrimaryReads,
    ) {
        #[cfg(feature = "sqlite")]
        {
            let sources = super::imp::prepare_sources(h, refs, before);
            super::imp::replace_prepared_in(&mut self.conn.lock().unwrap(), h, refs, watches, &sources);
        }
        #[cfg(not(feature = "sqlite"))]
        let _ = (h, refs, watches, before);
    }

    pub(crate) fn persist_discovery(&mut self, prune: bool) {
        #[cfg(feature = "sqlite")]
        {
            crate::discover_cache::persist_in(&mut self.conn.lock().unwrap(), prune);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            crate::discover_cache::persist(prune);
        }
    }

    pub(crate) fn publish_all(&mut self, rows: &[Publication<'_>]) {
        #[cfg(feature = "sqlite")]
        {
            let sources: Vec<_> = rows
                .iter()
                .map(|(h, refs, _, before)| super::imp::prepare_sources(*h, refs, before))
                .collect();
            super::imp::publish_all_in(&mut self.conn.lock().unwrap(), rows, &sources);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = rows;
        }
    }
}
