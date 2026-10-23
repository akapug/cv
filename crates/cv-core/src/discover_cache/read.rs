//! An operation capability, propagated explicitly to discovery workers. No global connection.
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "sqlite")]
use std::sync::OnceLock;
#[cfg(feature = "sqlite")]
type Connection = Arc<std::sync::Mutex<rusqlite::Connection>>;
type WriterKey = (crate::Harness, String, PathBuf);
#[cfg(all(test, feature = "sqlite"))]
#[derive(Default)]
struct Counters {
    opens: std::sync::atomic::AtomicUsize,
    lookups: std::sync::atomic::AtomicUsize,
    rows: std::sync::atomic::AtomicUsize,
}
struct Owner {
    writer_versions: std::sync::Mutex<std::collections::HashMap<WriterKey, Option<String>>>,
    /// The catalog this operation shares with its workers; `None` when there is no catalog home.
    path: Option<PathBuf>,
    #[cfg(feature = "sqlite")]
    connection: OnceLock<Option<Connection>>,
    cache: std::sync::OnceLock<Arc<super::Cache>>,
    #[cfg(all(test, feature = "sqlite"))]
    counters: Counters,
}
#[derive(Clone)]
pub(crate) struct Context(Arc<Owner>);
thread_local! {
    static CURRENT: RefCell<Vec<(u64,Context)>> = const { RefCell::new(Vec::new()) };
    static NEXT_SCOPE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub(crate) struct Scope(u64, std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            let mut scopes = current.borrow_mut();
            if let Some(index) = scopes.iter().position(|(id, _)| *id == self.0) {
                scopes.remove(index);
            }
        });
    }
}
impl Context {
    pub(crate) fn inherited() -> Option<Self> {
        CURRENT.with(|current| current.borrow().last().map(|(_, context)| context.clone()))
    }
    pub(crate) fn current() -> Self {
        Self::inherited().unwrap_or_else(|| {
            Self(Arc::new(Owner {
                writer_versions: Default::default(),
                #[cfg(feature = "sqlite")]
                path: crate::catalog::db_path().map(|path| catalog_key(&path)),
                #[cfg(not(feature = "sqlite"))]
                path: None,
                #[cfg(feature = "sqlite")]
                connection: OnceLock::new(),
                cache: std::sync::OnceLock::new(),
                #[cfg(all(test, feature = "sqlite"))]
                counters: Default::default(),
            }))
        })
    }
    #[cfg(feature = "sqlite")]
    pub(crate) fn from_connection(connection: Connection) -> Self {
        let key = catalog_key(std::path::Path::new(
            connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .path()
                .unwrap_or(""),
        ));
        Self(Arc::new(Owner {
            writer_versions: Default::default(),
            // An unnamed in-memory connection carries no catalog home.
            path: (!key.as_os_str().is_empty()).then_some(key),
            connection: OnceLock::from(Some(connection)),
            cache: std::sync::OnceLock::new(),
            #[cfg(all(test, feature = "sqlite"))]
            counters: Default::default(),
        }))
    }
    /// Observation lifetime is this discovery operation, shared with its parallel workers.
    pub(crate) fn observe_writer(&self, r: &crate::SessionRef, version: Option<&str>) {
        let mut rows = self.0.writer_versions.lock().unwrap();
        let newest = rows.entry((r.harness, r.id.clone(), r.path.clone())).or_default();
        crate::writer_version::observe(newest, version);
    }
    pub(crate) fn writer_versions(&self, harness: crate::Harness) -> Vec<(String, String, Option<String>)> {
        self.0
            .writer_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|((h, _, _), _)| *h == harness)
            .map(|((_, id, path), version)| (id.clone(), path.to_string_lossy().into_owned(), version.clone()))
            .collect()
    }
    pub(crate) fn path(&self) -> Option<&std::path::Path> {
        self.0.path.as_deref()
    }
    pub(crate) fn enter(&self) -> Scope {
        let id = NEXT_SCOPE.with(|next| {
            let id = next.get();
            next.set(id.checked_add(1).expect("discovery scope overflow"));
            id
        });
        CURRENT.with(|current| current.borrow_mut().push((id, self.clone())));
        Scope(id, std::marker::PhantomData)
    }
    pub(crate) fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let _scope = self.enter();
        f()
    }
    pub(super) fn cache(&self) -> Arc<super::Cache> {
        let mut built = None;
        let cache = self
            .0
            .cache
            .get_or_init(|| {
                let path = self.0.path.as_deref().unwrap_or(std::path::Path::new(""));
                let cache = super::cache_for(path);
                // Pin a newly built catalog cache to the environment, so it stays alive while
                // this home is in use ([`crate::env::Env::retained`]), not just while this
                // context lives.
                built = Some(Arc::new((path.to_path_buf(), cache.clone())));
                cache
            })
            .clone();
        if let Some(pinned) = built {
            *crate::env::Env::current().retained().lock().unwrap() = Some(pinned);
        }
        cache
    }
    #[cfg(feature = "sqlite")]
    pub(super) fn cached(&self) -> Option<Arc<super::Cache>> {
        self.0
            .cache
            .get()
            .cloned()
            .or_else(|| super::existing_cache(self.0.path.as_deref().unwrap_or(std::path::Path::new(""))))
    }
    #[cfg(all(test, feature = "sqlite"))]
    pub(super) fn note_lookup(&self) {
        self.0
            .counters
            .lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(all(test, feature = "sqlite"))]
    pub(super) fn note_row(&self) {
        self.0.counters.rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(all(test, feature = "sqlite"))]
    pub(super) fn work(&self) -> (usize, usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.0.counters.opens.load(Relaxed),
            self.0.counters.lookups.load(Relaxed),
            self.0.counters.rows.load(Relaxed),
        )
    }
    #[cfg(feature = "sqlite")]
    pub(crate) fn with_connection<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> T) -> Option<T> {
        self.with_connection_mut(|conn| f(conn))
    }
    #[cfg(feature = "sqlite")]
    pub(crate) fn with_connection_mut<T>(&self, f: impl FnOnce(&mut rusqlite::Connection) -> T) -> Option<T> {
        let connection = self.connection()?;
        let mut connection = connection.lock().inspect_err(super::note).ok()?;
        Some(f(&mut connection))
    }
    #[cfg(feature = "sqlite")]
    fn connection(&self) -> Option<Connection> {
        self.0
            .connection
            .get_or_init(|| {
                let path = self.0.path.as_deref()?;
                #[cfg(test)]
                self.0.counters.opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::catalog::open_at(path).map(|connection| Arc::new(std::sync::Mutex::new(connection)))
            })
            .clone()
    }
    /// Reuse the operation's existing catalog connection after scan-cache locks have dropped.
    /// Refilling by logical session key also covers directory refs cached under a child file.
    pub(crate) fn refill_turns(&self, refs: &mut [crate::SessionRef]) {
        if refs.iter().all(|r| r.message_count.is_some()) {
            return;
        }
        #[cfg(feature = "sqlite")]
        if let Some(connection) = self.connection() {
            crate::catalog::Reader::from_connection(connection).refill_turns(refs.iter_mut());
        }
        #[cfg(not(feature = "sqlite"))]
        let _ = refs;
    }
}

pub(super) fn catalog_key(path: &std::path::Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    path.canonicalize().unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .or_else(|| std::path::absolute(path).ok())
            .unwrap_or_else(|| path.to_path_buf())
    })
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::discover_cache::{cached_scan, cached_scan_many, stat_key, INTERPRETATION_VERSION};

    fn row(conn: &rusqlite::Connection, path: &std::path::Path, id: &str) {
        let reference = crate::SessionRef::new(id, crate::Harness::Claude, path.to_path_buf());
        let mut reads = crate::source::ReadSet::default();
        reads.record(path);
        let (mtime, size) = stat_key(path).unwrap();
        conn.execute("INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs,source_reads,interpretation_version) VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![path.to_string_lossy(),mtime as i64,size as i64,
                serde_json::to_string(&vec![reference]).unwrap(),serde_json::to_string(&reads).unwrap(),INTERPRETATION_VERSION]).unwrap();
    }
    fn owner(
        path: &std::path::Path,
        source: &std::path::Path,
        id: &str,
    ) -> (Context, std::sync::Weak<std::sync::Mutex<rusqlite::Connection>>) {
        let connection = crate::catalog::open_at(path).unwrap();
        row(&connection, source, id);
        let connection = Arc::new(std::sync::Mutex::new(connection));
        let weak = Arc::downgrade(&connection);
        (Context::from_connection(connection), weak)
    }

    #[test]
    fn scoped_capability_restores_nested_and_early_drop_and_closes_connection() {
        let world = crate::test_world("scan-scopes");
        let source = world.base.join("source");
        std::fs::write(&source, "metadata").unwrap();
        let (a, weak_a) = owner(&world.base.join("a.db"), &source, "a");
        let (b, weak_b) = owner(&world.base.join("b.db"), &source, "b");
        let outer = a.enter();
        assert_eq!(cached_scan(&source, || panic!("a must hit")).unwrap().id, "a");
        assert_eq!(
            a.work(),
            (0, 1, 1),
            "operation reader supplies its initialized connection"
        );
        let inner = b.enter();
        assert_eq!(cached_scan(&source, || panic!("b must hit")).unwrap().id, "b");
        drop(outer); // An earlier guard cannot remove the later active scope.
        assert_eq!(Context::inherited().unwrap().path(), b.path());
        drop(inner);
        assert!(Context::inherited().is_none());
        a.run(|| {
            let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.run(|| panic!("expected"))));
            assert!(failure.is_err());
            assert_eq!(Context::inherited().unwrap().path(), a.path());
        });
        assert!(Context::inherited().is_none());
        let old_cache = Arc::downgrade(&a.cache());
        b.cache(); // Only the current home remains strongly retained globally.
        drop(a);
        assert!(old_cache.upgrade().is_none());
        assert!(weak_a.upgrade().is_none());
        drop(b);
        assert!(
            weak_b.upgrade().is_none(),
            "cache entries do not retain catalog connections"
        );
    }

    #[test]
    fn concurrent_operations_route_each_home_and_parallel_worker_explicitly() {
        let world = crate::test_world("scan-overlap");
        let source = world.base.join("source");
        std::fs::write(&source, "metadata").unwrap();
        let (a, _) = owner(&world.base.join("a.db"), &source, "a");
        let (b, _) = owner(&world.base.join("b.db"), &source, "b");
        // A later ambient home cannot redirect an already captured operation capability.
        world.scope.set("CLUSTERVISION_HOME", world.base.join("another-home"));

        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            for (context, id) in [(&a, "a"), (&b, "b")] {
                let source = &source;
                let barrier = &barrier;
                threads.spawn(move || {
                    context.run(|| {
                        barrier.wait();
                        let ids = crate::par_discovery_filter_map((0..32).collect(), |_| {
                            Some(cached_scan(source, || panic!("must hit scoped home")).unwrap().id)
                        });
                        assert_eq!(ids, vec![id.to_string(); 32]);
                        assert_eq!(Context::inherited().unwrap().path(), context.path());
                    })
                });
            }
        });
        assert!(Context::inherited().is_none());
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn rayon_callback_panic_restores_previous_capability() {
        let world = crate::test_world("scan-panic");
        let source = world.base.join("source");
        std::fs::write(&source, "metadata").unwrap();
        let (a, _) = owner(&world.base.join("a.db"), &source, "a");
        let (b, _) = owner(&world.base.join("b.db"), &source, "b");
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        pool.install(|| {
            a.run(|| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::par_discovery_filter_map(vec![0], |_| {
                        b.run(|| -> Option<usize> { panic!("expected callback panic") })
                    });
                }));
                assert!(result.is_err());
                assert_eq!(Context::inherited().unwrap().path(), a.path());
            })
        });
        pool.install(|| assert!(Context::inherited().is_none(), "worker has no leaked capability"));
    }

    #[test]
    fn standalone_batch_opens_once_and_only_demands_matching_rows() {
        let world = crate::test_world("scan-batch");
        let conn = crate::catalog::open_db().unwrap();
        let paths: Vec<_> = (0..64)
            .map(|i| {
                let path = world.base.join(format!("source-{i}"));
                std::fs::write(&path, "metadata").unwrap();
                row(&conn, &path, &format!("id-{i}"));
                path
            })
            .collect();
        drop(conn);
        let context = Context::current();
        assert!(context.0.connection.get().is_none());
        assert!(context.0.cache.get().is_none());
        // A discovery callback which never uses the cache initializes neither DB nor cache.
        context.run(|| assert_eq!(crate::par_discovery_filter_map(vec![1], Some), vec![1]));
        assert_eq!(context.work(), (0, 0, 0));
        assert!(context.0.connection.get().is_none());
        for _ in 0..2 {
            context.run(|| {
                let mut ids = crate::par_discovery_filter_map(paths.clone(), |path| {
                    Some(cached_scan(&path, || panic!("persisted row must hit")).unwrap().id)
                });
                ids.sort();
                assert_eq!(ids.len(), 64);
            });
            assert_eq!(context.work(), (1, 64, 64), "repeat process hits perform no SQL lookup");
        }
        let miss = world.base.join("unsupported");
        std::fs::write(&miss, "metadata").unwrap();
        context.run(|| {
            for _ in 0..4 {
                assert!(cached_scan_many(&miss, Vec::new).is_empty());
            }
        });
        assert_eq!(
            context.work(),
            (1, 68, 64),
            "empty scans stay uncached and perform repeated keyed misses"
        );
        assert!(!context.cache().entries.read().unwrap().contains_key(&miss));
        eprintln!(
            "standalone scan work opens=1,keyed_lookups=68,returned_rows=64; 64 hits, repeat batch, four empty misses"
        );
    }
}
