//! Indexed adapter facts in a private temporary database, never in a native store.
use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction};
use std::sync::Mutex;

/// The source inventory owns this index and its original read set. SQLite owns/removes the
/// anonymous temporary file; no native connection, snapshot or second provenance is retained.
/// The page cache is bounded, but SQLite may still allocate an individual giant bound cell.
pub(crate) struct Selection(Mutex<Connection>);
impl Selection {
    // Charge page cache plus connection/schema overhead to the shared inventory admission.
    // This is an admission estimate, not a bound on SQLite's native allocator.
    pub(crate) const RETAINED_BYTES: usize = 1024 * 1024;

    pub(crate) fn build(schema: &str, load: impl FnOnce(&Transaction<'_>) -> Result<()>) -> Result<Self> {
        // An empty filename is SQLite's private, automatically deleted disk-backed database.
        let mut connection = Connection::open("").context("opening private selection")?;
        connection.execute_batch("PRAGMA cache_size=-256; PRAGMA mmap_size=0; PRAGMA journal_mode=OFF;")?;
        connection.execute_batch(schema)?;
        let transaction = connection.transaction()?;
        load(&transaction)?;
        transaction.commit()?;
        Ok(Self(Mutex::new(connection)))
    }

    pub(crate) fn with<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("private selection lock poisoned"))?;
        operation(&connection)
    }
}
