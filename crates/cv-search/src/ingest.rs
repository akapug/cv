//! Raw document admission spans both the producer channel and Tantivy's indexing pipeline.
//!
//! Tantivy's arena budget excludes its 10,000-document input queue. Each document owns a byte
//! reservation until Tantivy finishes consuming it, so moving between queues cannot evade this
//! bound. Parser working sets, worker body buffers, index arenas and merges are separate costs.
use anyhow::{Context, Result};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Condvar, Mutex};
use tantivy::schema::Document;
use tantivy::{Index, IndexWriter, Opstamp, TantivyDocument};

pub(super) const DOCUMENT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct State {
    bytes: usize,
    peak: usize,
    largest: usize,
    waits: usize,
    closed: bool,
}

pub(super) struct Admission {
    limit: usize,
    state: Mutex<State>,
    wake: Condvar,
}
impl Admission {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        })
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation> {
        let mut state = self.state.lock().unwrap();
        // An oversized document runs alone. Rejecting it would silently change what is indexed;
        // charging its actual allocation preserves semantics and prevents concurrent oversize.
        while !state.closed && state.bytes != 0 && bytes > self.limit.saturating_sub(state.bytes) {
            state.waits += 1;
            state = self.wake.wait(state).unwrap();
        }
        anyhow::ensure!(!state.closed, "index document admission closed");
        state.bytes = state
            .bytes
            .checked_add(bytes)
            .context("index document byte count overflow")?;
        state.peak = state.peak.max(state.bytes);
        state.largest = state.largest.max(bytes);
        Ok(Reservation {
            admission: Arc::clone(self),
            bytes,
        })
    }

    pub(super) fn document(self: &Arc<Self>, document: TantivyDocument) -> Result<AdmittedDocument> {
        let reservation = self.reserve(document_bytes(&document))?;
        Ok(AdmittedDocument {
            document,
            _reservation: reservation,
        })
    }

    pub(super) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.wake.notify_all();
    }

    #[cfg(test)]
    pub(super) fn stats(&self) -> (usize, usize, usize, usize) {
        let state = self.state.lock().unwrap();
        (state.bytes, state.peak, state.largest, state.waits)
    }
}

/// The compact document owns one public byte vector and a private field-address vector. Charge
/// the exact byte capacity plus twice the field count at a conservative address size (the private
/// packed address is smaller). Include both vector headers, wrapper and allocation overhead.
fn document_bytes(document: &TantivyDocument) -> usize {
    document
        .node_data
        .capacity()
        .saturating_add(
            document
                .len()
                .max(4)
                .saturating_mul(2 * std::mem::size_of::<(usize, usize)>()),
        )
        .saturating_add(std::mem::size_of::<AdmittedDocument>() + 128)
}

struct Reservation {
    admission: Arc<Admission>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.admission.state.lock().unwrap().bytes -= self.bytes;
        self.admission.wake.notify_all();
    }
}

// Field order matters: free the document's allocations before returning its reservation.
pub(super) struct AdmittedDocument {
    document: TantivyDocument,
    _reservation: Reservation,
}
#[cfg(test)]
impl AdmittedDocument {
    pub(super) fn inner(&self) -> &TantivyDocument {
        &self.document
    }
}
impl Document for AdmittedDocument {
    type Value<'a> = <TantivyDocument as Document>::Value<'a>;
    type FieldsValuesIter<'a> = <TantivyDocument as Document>::FieldsValuesIter<'a>;
    fn iter_fields_and_values(&self) -> Self::FieldsValuesIter<'_> {
        self.document.iter_fields_and_values()
    }
}

/// Every write path, including single-session/test indexing, uses the same raw-document budget.
pub(super) struct Writer {
    inner: IndexWriter<AdmittedDocument>,
    pub(super) admission: Arc<Admission>,
    #[cfg(test)]
    fail_after: Mutex<Option<usize>>,
}
impl Writer {
    pub(super) fn new(index: &Index, heap: usize) -> Result<Self> {
        let inner = index.writer(heap).context("creating tantivy index writer")?;
        #[cfg(all(test, feature = "probe-heap"))]
        let limit = if std::env::var_os("CV_SWEEP_COMPARE_UNBOUNDED").is_some() {
            usize::MAX
        } else {
            DOCUMENT_BYTES
        };
        #[cfg(not(all(test, feature = "probe-heap")))]
        let limit = DOCUMENT_BYTES;
        Ok(Self {
            inner,
            admission: Admission::new(limit),
            #[cfg(test)]
            fail_after: Mutex::new(None),
        })
    }
    #[cfg(test)]
    pub(super) fn add_document(&self, document: TantivyDocument) -> Result<Opstamp> {
        self.add_admitted(self.admission.document(document)?)
    }
    pub(super) fn add_admitted(&self, document: AdmittedDocument) -> Result<Opstamp> {
        #[cfg(test)]
        if let Some(remaining) = self.fail_after.lock().unwrap().as_mut() {
            anyhow::ensure!(*remaining != 0, "injected index consumer failure");
            *remaining -= 1;
        }
        self.inner.add_document(document).map_err(Into::into)
    }
    #[cfg(test)]
    pub(super) fn document_limit(&mut self, bytes: usize) {
        self.admission = Admission::new(bytes);
    }
    #[cfg(test)]
    pub(super) fn fail_after(&self, documents: usize) {
        *self.fail_after.lock().unwrap() = Some(documents);
    }
    /// Close the writer and join the merge/GC work its commits started.
    #[cfg(test)]
    pub(super) fn wait_merging_threads(self) -> Result<()> {
        self.inner.wait_merging_threads().map_err(Into::into)
    }
}

impl Deref for Writer {
    type Target = IndexWriter<AdmittedDocument>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for Writer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn document_admission_follows_the_payload_and_close_wakes_waiters() {
        let admission = Admission::new(128);
        let first = admission.reserve(96).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let admission = Arc::clone(&admission);
            scope.spawn(move || tx.send(admission.reserve(64)).unwrap());
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            drop(first);
            let next = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(next.admission.stats().0, 64);
            // Moving a reservation through a queue cannot return its bytes early.
            let (tx, rx) = mpsc::sync_channel(1);
            tx.send(next).unwrap();
            assert_eq!(admission_stats_from_receiver(&rx).0, 0);
        });
        assert_eq!(admission.stats().0, 0);
        assert!(admission.stats().1 <= 128);

        let oversized = admission.reserve(256).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let other = Arc::clone(&admission);
            scope.spawn(move || tx.send(other.reserve(1)).unwrap());
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            admission.close();
            assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
            drop(oversized);
        });
        assert_eq!(admission.stats().0, 0);
        assert_eq!(admission.stats().1, 256);
    }

    fn admission_stats_from_receiver(rx: &mpsc::Receiver<Reservation>) -> (usize, usize, usize, usize) {
        let reservation = rx.recv().unwrap();
        let admission = Arc::clone(&reservation.admission);
        assert_eq!(admission.stats().0, reservation.bytes);
        drop(reservation);
        admission.stats()
    }

    #[test]
    fn tantivy_commit_and_writer_drop_return_all_document_reservations() {
        let mut schema = tantivy::schema::Schema::builder();
        let body = schema.add_text_field("body", tantivy::schema::TEXT);
        let index = Index::create_in_ram(schema.build());
        let mut writer = Writer::new(&index, 15_000_000).unwrap();
        writer.document_limit(1024 * 1024);
        let admission = Arc::clone(&writer.admission);
        for _ in 0..40 {
            let mut document = TantivyDocument::default();
            document.add_text(body, "x".repeat(600_000));
            writer.add_document(document).unwrap();
        }
        writer.commit().unwrap();
        assert_eq!(admission.stats().0, 0);
        assert!(admission.stats().1 <= admission.limit.max(admission.stats().2));
        assert_eq!(index.reader().unwrap().searcher().num_docs(), 40);
        writer.fail_after(0);
        let mut document = TantivyDocument::default();
        document.add_text(body, "failed");
        assert!(writer.add_document(document).is_err());
        assert_eq!(admission.stats().0, 0);
        drop(writer);
        assert_eq!(admission.stats().0, 0);
    }
}
