//! Per-session discovery listings of a shared store, kept by each session's certificate.
use crate::source::{Payload, Rows};
use crate::{Harness, SessionRef};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What one adapter lists of a session of a shared store, from the session's own rows.
pub(crate) trait Listing:
    serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static
{
    /// The catalog source-record consumer the listings are kept under.
    const TAG: &'static str;
}

/// Each session's listing with the [`Rows`] certificate its rows had when it was decoded, for one
/// store (by device and inode: a replaced store is another store). A write to the store moves its
/// identity for every session, so discovery runs again; a session whose certificate holds keeps
/// its listing and only the sessions whose rows moved are decoded. Kept in this process and as a
/// catalog source record (the store-wide consumer `T::TAG`, empty session id), read back whatever
/// the store's state, since each session is admitted by its own certificate.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Listings<T> {
    store: Option<(u64, u64)>,
    sessions: HashMap<String, (Rows, T)>,
}

impl<T: Listing> Payload for Listings<T> {
    const TAG: &'static str = T::TAG;
}

type Memo = Mutex<HashMap<(Harness, PathBuf), Arc<dyn std::any::Any + Send + Sync>>>;
static MEMO: std::sync::LazyLock<Memo> = std::sync::LazyLock::new(Default::default);

fn identity(db: &Path) -> Option<(u64, u64)> {
    crate::source::FileId::at(db).map(|id| (id.dev, id.ino))
}

fn key(harness: Harness, db: &Path) -> SessionRef {
    SessionRef::new(
        String::new(),
        harness,
        std::path::absolute(db).unwrap_or_else(|_| db.to_path_buf()),
    )
}

/// One discovery pass over a store: the listings kept from before, and the ones this pass lists.
pub(crate) struct Pass<T: Listing> {
    harness: Harness,
    db: PathBuf,
    previous: Option<Arc<Listings<T>>>,
    listed: Listings<T>,
}

impl<T: Listing> Pass<T> {
    /// Begin a pass over `db`, with the listings last kept for it: this process's, else the
    /// catalog's.
    pub(crate) fn begin(harness: Harness, db: &Path) -> Self {
        let store = identity(db);
        let memo = MEMO
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(harness, db.to_path_buf()))
            .cloned()
            .and_then(|any| any.downcast::<Listings<T>>().ok());
        let previous = memo
            .or_else(|| {
                crate::discover_cache::read::Context::current()
                    .with_connection(|conn| crate::source::record_in::<Listings<T>>(conn, T::TAG, &key(harness, db)))
                    .flatten()
                    .map(|record| Arc::new(record.payload))
            })
            .filter(|previous| previous.store == store);
        Self {
            harness,
            db: db.to_path_buf(),
            previous,
            listed: Listings {
                store,
                sessions: HashMap::new(),
            },
        }
    }

    /// `id`'s listing: the kept one when its certificate is `certificate`, else `decode`'s.
    /// A session without a certificate is decoded and not kept.
    pub(crate) fn list<E>(
        &mut self,
        id: &str,
        certificate: Option<Rows>,
        decode: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let kept = self
            .previous
            .as_ref()
            .and_then(|previous| previous.sessions.get(id))
            .filter(|(then, _)| certificate == Some(*then))
            .map(|(_, listing)| listing.clone());
        let listing = match kept {
            Some(listing) => listing,
            None => {
                #[cfg(test)]
                DECODED.with(|n| n.set(n.get() + 1));
                decode()?
            }
        };
        if let Some(certificate) = certificate {
            self.listed
                .sessions
                .insert(id.to_owned(), (certificate, listing.clone()));
        }
        Ok(listing)
    }

    /// Keep this pass's listings in this process and the catalog (a frozen or unavailable
    /// catalog keeps none there).
    pub(crate) fn keep(self) {
        let r = key(self.harness, &self.db);
        let baseline = crate::source::Probe::default()
            .snapshot(&r)
            .with_reads(&Default::default());
        crate::discover_cache::read::Context::current().with_connection_mut(|conn| {
            let mut publish = || -> anyhow::Result<()> {
                let tx = conn.transaction()?;
                crate::source::publish_in(
                    &tx,
                    T::TAG,
                    &r,
                    &baseline,
                    &self.listed,
                    &mut crate::source::Probe::default(),
                )?;
                tx.commit()?;
                Ok(())
            };
            if let Err(error) = publish() {
                crate::env::warn_once("the store listing catalog", error);
            }
        });
        MEMO.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((self.harness, self.db), Arc::new(self.listed));
    }
}

/// Forget this process's listings, as a new process starts (tests).
#[cfg(test)]
pub(crate) fn forget() {
    MEMO.lock().unwrap().clear();
}

#[cfg(test)]
thread_local! {
    /// Sessions a pass decoded on this thread.
    pub(crate) static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
