//! Typed, bounded metadata inventories owned by one multi-source operation.
use super::ReadSet;
use anyhow::Result;
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

/// What one namespace may retain in one operation. Per namespace, so a projection kept per small
/// file (OpenCode's message ids) cannot crowd out a shared inventory other sessions reuse (Qwen's
/// scheduled tasks, OpenClaw's registries), which would then reload once per session.
const RETAINED_BYTES: usize = 16 * 1024 * 1024;
/// Per-entry hash-map and path overhead counted alongside retained bytes, so many small
/// entries cannot bypass admission.
const ENTRY_BYTES: usize = 256;
#[derive(Hash, PartialEq, Eq)]
struct Key(TypeId, &'static str, PathBuf);
struct Cached {
    value: Arc<dyn Any + Send + Sync>,
    reads: ReadSet,
    bytes: usize,
}
type Slot = Arc<Mutex<Option<Cached>>>;
#[derive(Default)]
struct State {
    entries: HashMap<Key, Slot>,
    /// Retained bytes per namespace.
    bytes: HashMap<&'static str, usize>,
}

/// Say once per namespace that its inventories stopped being retained: each later use reloads.
fn over_budget(namespace: &'static str) {
    crate::env::warn_once(
        namespace,
        format_args!(
            "its retention passed {} MiB in this operation; each further use reloads it",
            RETAINED_BYTES >> 20
        ),
    );
}
#[derive(Clone, Default)]
pub(crate) struct Context(Arc<Mutex<State>>);
thread_local! {
    static CURRENT: RefCell<Weak<Mutex<State>>> = const { RefCell::new(Weak::new()) };
}
impl Context {
    pub(crate) fn current() -> Option<Self> {
        CURRENT.with(|current| current.borrow().upgrade().map(Self))
    }
    pub(super) fn operation() -> Self {
        Self::current().unwrap_or_else(|| {
            let context = Self::default();
            CURRENT.with(|current| *current.borrow_mut() = Arc::downgrade(&context.0));
            context
        })
    }
    pub(crate) fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Restore(Weak<Mutex<State>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|current| *current.borrow_mut() = self.0.clone());
            }
        }
        let prior = CURRENT.with(|current| current.replace(Arc::downgrade(&self.0)));
        let _restore = Restore(prior);
        f()
    }
}

pub(crate) fn inventory_scoped() -> bool {
    Context::current().is_some()
}

/// A caller captures baseline identities before reading, returns its typed metadata index,
/// complete consulted reads and an estimate of retained index bytes (including maximum growth
/// for interior-mutable indexes). Hits replay the original read set. Standalone calls load fresh;
/// oversized values remain usable but are not retained. Never retain native connections in T.
pub(crate) fn inventory<T: Send + Sync + 'static>(
    namespace: &'static str,
    key: &Path,
    baseline: &ReadSet,
    load: impl FnOnce() -> Result<(T, ReadSet, usize)>,
) -> Result<(Arc<T>, ReadSet)> {
    anyhow::ensure!(
        baseline.fresh(&mut super::Probe::default()),
        "inventory baseline changed before read"
    );
    let context = Context::current();
    let slot = context.as_ref().and_then(|context| {
        let key = Key(TypeId::of::<T>(), namespace, super::absolute(key)?);
        let mut state = context.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = state.entries.get(&key) {
            return Some(slot.clone());
        }
        // Count key/map/slot overhead too, so many empty inventories cannot bypass admission.
        let bytes = ENTRY_BYTES.saturating_add(key.2.as_os_str().len());
        let held = state.bytes.entry(namespace).or_default();
        if bytes > RETAINED_BYTES.saturating_sub(*held) {
            over_budget(namespace);
            return None;
        }
        let slot = Arc::new(Mutex::new(None));
        *held += bytes;
        state.entries.insert(key, slot.clone());
        Some(slot)
    });
    // Per-key single flight; the operation map lock never spans filesystem/database loading.
    let mut retained = slot
        .as_ref()
        .map(|slot| slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    if let Some(Some(cached)) = retained.as_deref() {
        if cached.reads.includes(baseline) && cached.reads.fresh(&mut super::Probe::default()) {
            if let Ok(value) = cached.value.clone().downcast::<T>() {
                return Ok((value, cached.reads.clone()));
            }
        }
    }
    let loaded = load();
    anyhow::ensure!(
        baseline.fresh(&mut super::Probe::default()),
        "inventory baseline changed during read"
    );
    let (value, loaded_reads, estimate) = loaded?;
    let mut reads = baseline.clone();
    reads.merge(&loaded_reads);
    anyhow::ensure!(
        reads.fresh(&mut super::Probe::default()),
        "inventory source changed during read"
    );
    let value = Arc::new(value);
    if let (Some(context), Some(retained)) = (context.as_ref(), retained.as_deref_mut()) {
        let guard_bytes = reads.entries.keys().fold(0usize, |bytes, path| {
            bytes.saturating_add(ENTRY_BYTES.saturating_add(path.as_os_str().len()))
        });
        let bytes = estimate.saturating_add(guard_bytes);
        let mut state = context.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let held = state.bytes.entry(namespace).or_default();
        if let Some(old) = retained.take() {
            *held = held.saturating_sub(old.bytes);
        }
        if bytes <= RETAINED_BYTES.saturating_sub(*held) {
            *held += bytes;
            *retained = Some(Cached {
                value: value.clone(),
                reads: reads.clone(),
                bytes,
            });
        } else {
            over_budget(namespace);
        }
    }
    Ok((value, reads))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn baseline(path: &Path) -> ReadSet {
        let mut reads = ReadSet::default();
        reads.record(path);
        reads
    }

    #[test]
    fn inventory_nested_parallel_mutation_and_lifetime_are_operation_owned() {
        let world = crate::test_world("source-inventory-operation");
        let path = world.base.join("registry");
        std::fs::write(&path, "old").unwrap();
        let calls = AtomicUsize::new(0);
        let get = || {
            let reads = baseline(&path);
            inventory("registry", &path, &reads, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((std::fs::read_to_string(&path)?, reads.clone(), 64))
            })
        };
        assert!(!inventory_scoped());
        get().unwrap();
        get().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "standalone reads must each load the inventory"
        );
        let scope = super::super::read_scope();
        let (first, reads) = get().unwrap();
        {
            let _nested = super::super::read_scope();
            assert!(Arc::ptr_eq(&first, &get().unwrap().0));
        }
        let values = crate::par_filter_map((0..16).collect(), |_| Some(get().unwrap()));
        assert!(values
            .iter()
            .all(|(value, guarded)| Arc::ptr_eq(value, &first) && guarded == &reads));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "nested/parallel workers reread inventory"
        );
        std::fs::write(&path, "changed").unwrap();
        assert_eq!(*get().unwrap().0, "changed");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        drop(scope);
        assert!(!inventory_scoped());
        let _next = super::super::read_scope();
        get().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "another operation retained old inventory"
        );
    }

    /// One namespace at its budget leaves another's retention alone: a shared inventory is still
    /// loaded once however many per-file projections came before it.
    #[test]
    fn a_full_namespace_does_not_evict_another() {
        let world = crate::test_world("source-inventory-namespaces");
        let (big, shared) = (world.base.join("big"), world.base.join("shared"));
        std::fs::write(&big, "big").unwrap();
        std::fs::write(&shared, "shared").unwrap();
        let _scope = super::super::read_scope();
        let reads = baseline(&big);
        // Exactly the whole budget: the entry's slot, its guard path and its estimate.
        let overhead = ENTRY_BYTES + super::super::absolute(&big).unwrap().as_os_str().len();
        inventory("test-per-file", &big, &reads, || {
            Ok((0usize, reads.clone(), RETAINED_BYTES - 2 * overhead))
        })
        .unwrap();
        let calls = AtomicUsize::new(0);
        let shared_reads = baseline(&shared);
        for _ in 0..3 {
            inventory("test-shared", &shared, &shared_reads, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((1usize, shared_reads.clone(), 64))
            })
            .unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the shared inventory was reloaded per use"
        );
    }

    #[test]
    fn inventory_absence_races_and_oversized_admission_keep_guards() {
        let world = crate::test_world("source-inventory-admission");
        let path = world.base.join("conditional");
        let _scope = super::super::read_scope();
        let absent = baseline(&path);
        let (_, returned) = inventory("conditional", &path, &absent, || Ok((false, absent.clone(), 1))).unwrap();
        assert_eq!(returned, absent, "original absence baseline was lost");
        std::fs::write(&path, "present").unwrap();
        let present = baseline(&path);
        assert!(
            *inventory("conditional", &path, &present, || Ok((true, present.clone(), 1)))
                .unwrap()
                .0
        );
        let raced = inventory("race", &path, &present, || {
            std::fs::write(&path, "changed").unwrap();
            Ok((1usize, present.clone(), 8))
        });
        assert!(raced.is_err(), "raced load certified a post-read stamp");
        let before = baseline(&path);
        let post_read_stamp = inventory("post-read-stamp", &path, &before, || {
            std::fs::write(&path, "newer mutation").unwrap();
            Ok((1usize, baseline(&path), 8))
        });
        assert!(
            post_read_stamp.is_err(),
            "loader replaced original baseline with post-read stamp"
        );
        let current = baseline(&path);
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            let (value, _) = inventory("oversized", &path, &current, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((7usize, current.clone(), RETAINED_BYTES + 1))
            })
            .unwrap();
            assert_eq!(*value, 7);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "oversized inventory was retained");
    }

    #[test]
    fn public_scope_run_shares_first_load_and_restores_worker_context() {
        let world = crate::test_world("source-inventory-explicit-workers");
        let path = world.base.join("source");
        std::fs::write(&path, "source").unwrap();
        let reads = baseline(&path);
        let parent = super::super::read_scope();
        let calls = AtomicUsize::new(0);
        std::thread::scope(|threads| {
            for n in 0..4 {
                let parent = &parent;
                let calls = &calls;
                let reads = &reads;
                let path = &path;
                threads.spawn(move || {
                    let _prior = super::super::read_scope();
                    inventory("explicit", path, reads, || Ok((n + 100usize, reads.clone(), 8))).unwrap();
                    let value = parent
                        .run(|| {
                            inventory("explicit", path, reads, || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok((42usize, reads.clone(), 8))
                            })
                        })
                        .unwrap()
                        .0;
                    assert_eq!(*value, 42);
                    assert_eq!(
                        *inventory::<usize>("explicit", path, reads, || panic!("worker context was not restored"))
                            .unwrap()
                            .0,
                        n + 100
                    );
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parallel_first_demand_singleflights_and_separate_operations_stay_isolated() {
        let world = crate::test_world("source-inventory-singleflight");
        let path = world.base.join("source");
        std::fs::write(&path, "source").unwrap();
        let reads = baseline(&path);
        let calls = AtomicUsize::new(0);
        let outer = super::super::read_scope();
        crate::par_filter_map((0..16).collect(), |_| {
            Some(
                inventory("first", &path, &reads, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok((42usize, reads.clone(), 8))
                })
                .unwrap(),
            )
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        std::thread::scope(|threads| {
            threads
                .spawn(|| {
                    assert!(!inventory_scoped());
                    let _separate = super::super::read_scope();
                    assert_eq!(
                        *inventory("first", &path, &reads, || Ok((9usize, reads.clone(), 8)))
                            .unwrap()
                            .0,
                        9
                    );
                })
                .join()
                .unwrap();
        });
        assert_eq!(
            *inventory::<usize>("first", &path, &reads, || panic!("outer context changed"))
                .unwrap()
                .0,
            42
        );
        drop(outer);
    }
}
