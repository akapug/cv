//! Opt-in, operation-owned aggregate work accounting. `CV_INDEX_STAGE_WORK=1` enables the CLI
//! index report on stderr; the default is off. No paths, keys or content enter this fixed schema.
//! CPU is exclusive CV-thread CPU, not time waited for Tantivy. The process residual includes
//! asynchronous and unassigned work; it is not attributed back to the worker that caused it.
//! Logical representation bytes and merge extents are deliberately not physical read bytes.
//! `selected_parse` includes adapter parsing, checkpoint construction and body/event/count
//! projection on CV workers; resume SQL/JSON and snapshot copies have nested exclusive stages.
//! JSONL visited bytes count framed input handed to codecs (decoded for compressed sources),
//! not all parser inputs. Search body bytes count projected body/context document submissions.
//! Stage ordinals and process-clock entry/exit bounds summarize repeated scopes, not a timeline;
//! their process intervals overlap and must not be added. Report generation itself is excluded.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

/// Named opt-in; absent, malformed and all other values leave accounting disabled.
pub const SETTING: &str = "CV_INDEX_STAGE_WORK";
pub const DEFAULT_ENABLED: bool = false;

macro_rules! categories {
    ($name:ident { $($item:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($item),+ }
        impl $name {
            const ALL: &'static [Self] = &[$(Self::$item),+];
            const COUNT: usize = Self::ALL.len();
        }
    };
}

categories!(Stage {
    Discovery,
    DiscoveryPublish,
    ScanCache,
    BaselineCounts,
    BaselineEvents,
    BaselineFts,
    BaselineOther,
    IndexedMetadata,
    Planning,
    EarlyPlanning,
    SelectedParse,
    Resume,
    SqliteSnapshot,
    Writer,
    Commit,
    Publication,
    CompactionPlan,
    Compaction,
    Other,
});

categories!(Metric {
    Calls,
    Rows,
    InputBytes,
    DecodedRows,
    StampInventoryCalls,
    StampInventoryBytes,
    StampInventoryEntries,
    StampReadCalls,
    StampReadBytes,
    StampReadEntries,
    Candidates,
    SelectedJobs,
    ScanHits,
    ScanMisses,
    ScanPrunePaths,
    SnapshotHits,
    SnapshotMisses,
    ImmutableOpens,
    DatabaseCopyBytes,
    WalCopyBytes,
    JournalCopyBytes,
    HeadDemands,
    StateDemands,
    CheckpointInputBytes,
    CheckpointOutputBytes,
    JsonlVisitedBytes,
    JsonlRecords,
    SearchBodyBytes,
    SearchDocuments,
    MergeAttempts,
    MergeCompleted,
    MergeInputExtentBytes,
    MergeOutputExtentBytes,
    ExtentUnknown,
    ExtentAbsent,
});

struct Totals {
    cpu: AtomicU64,
    scopes: AtomicU64,
    first: AtomicU64,
    last: AtomicU64,
    process_first: AtomicU64,
    process_last: AtomicU64,
    metrics: [AtomicU64; Metric::COUNT],
}

struct State {
    stages: [Totals; Stage::COUNT],
    ordinal: AtomicU64,
    active: AtomicU64,
    incomplete: AtomicBool,
    clock_failed: AtomicBool,
    process_start: Option<u64>,
}

impl State {
    fn new() -> Self {
        Self {
            stages: std::array::from_fn(|_| Totals {
                first: AtomicU64::new(u64::MAX),
                process_first: AtomicU64::new(u64::MAX),
                cpu: AtomicU64::new(0),
                scopes: AtomicU64::new(0),
                last: AtomicU64::new(0),
                process_last: AtomicU64::new(0),
                metrics: std::array::from_fn(|_| AtomicU64::new(0)),
            }),
            ordinal: AtomicU64::new(0),
            active: AtomicU64::new(0),
            incomplete: AtomicBool::new(false),
            clock_failed: AtomicBool::new(false),
            process_start: clock(false),
        }
    }

    fn process_elapsed(&self) -> Option<u64> {
        let elapsed = clock(false)
            .zip(self.process_start)
            .and_then(|(now, start)| now.checked_sub(start));
        if elapsed.is_none() {
            self.clock_failed.store(true, Relaxed);
        }
        elapsed
    }
}

#[derive(Clone, Copy)]
struct Frame {
    stage: Stage,
    since: Option<u64>,
}

#[derive(Default)]
struct Thread {
    state: Option<Arc<State>>,
    frame: Option<Frame>,
}

thread_local! {
    static CURRENT: RefCell<Thread> = const { RefCell::new(Thread { state: None, frame: None }) };
    #[cfg(test)]
    static CLOCK_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Captured by the existing environment and source-operation scopes, never process-global.
#[derive(Clone, Default)]
pub(crate) struct Context(Option<Arc<State>>);

impl Context {
    pub(crate) fn current() -> Self {
        CURRENT.with(|slot| Self(slot.borrow().state.clone()))
    }

    pub(crate) fn enter(&self) -> Entered {
        CURRENT.with(|slot| {
            let mut current = slot.borrow_mut();
            if current
                .state
                .as_ref()
                .zip(self.0.as_ref())
                .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
                || (current.state.is_none() && self.0.is_none())
            {
                return Entered(None);
            }
            let now = current.state.as_ref().and_then(|_| clock(true));
            settle(&current, now);
            let previous = std::mem::replace(
                &mut *current,
                Thread {
                    state: self.0.clone(),
                    frame: None,
                },
            );
            Entered(Some(previous))
        })
    }
}

pub(crate) struct Entered(Option<Thread>);
impl Drop for Entered {
    fn drop(&mut self) {
        if let Some(mut previous) = self.0.take() {
            if previous.state.is_some() {
                if let Some(frame) = &mut previous.frame {
                    frame.since = clock(true);
                }
            }
            CURRENT.with(|slot| *slot.borrow_mut() = previous);
        }
    }
}

fn settle(current: &Thread, now: Option<u64>) {
    if let (Some(state), Some(frame)) = (&current.state, current.frame) {
        match frame.since.zip(now).and_then(|(start, end)| end.checked_sub(start)) {
            Some(ns) => {
                state.stages[frame.stage as usize].cpu.fetch_add(ns, Relaxed);
            }
            None => state.clock_failed.store(true, Relaxed),
        }
    }
}

/// An exclusive thread scope; fixed stages may nest, but their CPU does not double-count.
/// No report allocation or clock call occurs when disabled. Bound to the entering thread.
pub struct Scope {
    inner: Option<(Arc<State>, Option<Frame>)>,
    _thread: PhantomData<*const ()>,
}

pub fn enter(stage: Stage) -> Scope {
    let inner = CURRENT.with(|slot| {
        let mut current = slot.borrow_mut();
        let state = current.state.clone()?;
        let now = clock(true);
        settle(&current, now);
        let previous = current.frame.replace(Frame { stage, since: now });
        let ordinal = state.ordinal.fetch_add(1, Relaxed) + 1;
        let totals = &state.stages[stage as usize];
        totals.scopes.fetch_add(1, Relaxed);
        totals.first.fetch_min(ordinal, Relaxed);
        totals.last.fetch_max(ordinal, Relaxed);
        if let Some(cpu) = state.process_elapsed() {
            totals.process_first.fetch_min(cpu, Relaxed);
        }
        state.active.fetch_add(1, Relaxed);
        Some((state, previous))
    });
    Scope {
        inner,
        _thread: PhantomData,
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((state, mut previous)) = self.inner.take() {
            let now = clock(true);
            CURRENT.with(|slot| {
                let mut current = slot.borrow_mut();
                settle(&current, now);
                if let Some(frame) = &mut previous {
                    frame.since = now;
                }
                if let Some(cpu) = state.process_elapsed() {
                    if let Some(frame) = current.frame {
                        state.stages[frame.stage as usize].process_last.fetch_max(cpu, Relaxed);
                    }
                }
                current.frame = previous;
            });
            state.active.fetch_sub(1, Relaxed);
        }
    }
}

/// Whether this operation opted in; useful for observations requiring extra metadata access.
pub fn enabled() -> bool {
    CURRENT.with(|slot| slot.borrow().state.is_some())
}

/// The fixed category, when a parallel helper must carry a thread's attribution with its work.
#[cfg(any(feature = "parallel", test))]
pub(crate) fn current_stage() -> Option<Stage> {
    CURRENT.with(|slot| slot.borrow().frame.map(|frame| frame.stage))
}

/// Numeric hooks only: bounded atomic aggregation, no item keys, locks, clocks or extra decoding.
pub fn add(metric: Metric, amount: u64) {
    CURRENT.with(|slot| {
        let current = slot.borrow();
        if let Some(state) = &current.state {
            let stage = current.frame.map_or(Stage::Other, |frame| frame.stage);
            state.stages[stage as usize].metrics[metric as usize].fetch_add(amount, Relaxed);
        }
    });
}

pub fn incomplete() {
    CURRENT.with(|slot| {
        if let Some(state) = &slot.borrow().state {
            state.incomplete.store(true, Relaxed);
        }
    });
}

#[derive(serde::Serialize)]
struct StageReport {
    stage: Stage,
    scopes: u64,
    first_ordinal: Option<u64>,
    last_ordinal: Option<u64>,
    exclusive_thread_cpu_ns: u64,
    // These are overlapping process-clock boundary controls, NEVER per-stage attribution.
    process_cpu_first_entry_ns: Option<u64>,
    process_cpu_last_exit_ns: Option<u64>,
    work: Vec<(Metric, u64)>,
}

#[derive(serde::Serialize)]
struct Report<'a> {
    schema: u32,
    boundary: &'static str,
    source: Option<&'a str>,
    invocation: Option<String>,
    returned_success: bool,
    complete: bool,
    cpu_complete: bool,
    process_cpu_ns: Option<u64>,
    accounted_thread_cpu_ns: u64,
    async_and_unassigned_cpu_ns: Option<u64>,
    metadata_input_bytes: Option<u64>,
    non_jsonl_parse_bytes: Option<u64>,
    physical_read_bytes: Option<u64>,
    stages: Vec<StageReport>,
}

fn report<'a>(state: &State, source: Option<&'a str>, success: bool) -> Report<'a> {
    let process_cpu_ns = state.process_elapsed();
    let stages: Vec<_> = Stage::ALL
        .iter()
        .map(|&stage| {
            let t = &state.stages[stage as usize];
            let scopes = t.scopes.load(Relaxed);
            StageReport {
                stage,
                scopes,
                first_ordinal: (scopes != 0).then(|| t.first.load(Relaxed)),
                last_ordinal: (scopes != 0).then(|| t.last.load(Relaxed)),
                exclusive_thread_cpu_ns: t.cpu.load(Relaxed),
                process_cpu_first_entry_ns: (t.process_first.load(Relaxed) != u64::MAX)
                    .then(|| t.process_first.load(Relaxed)),
                process_cpu_last_exit_ns: (scopes != 0 && !state.clock_failed.load(Relaxed))
                    .then(|| t.process_last.load(Relaxed)),
                work: Metric::ALL
                    .iter()
                    .map(|&metric| (metric, t.metrics[metric as usize].load(Relaxed)))
                    .collect(),
            }
        })
        .collect();
    let accounted_thread_cpu_ns = stages.iter().map(|s| s.exclusive_thread_cpu_ns).sum();
    let residual = process_cpu_ns.and_then(|cpu| cpu.checked_sub(accounted_thread_cpu_ns));
    let cpu_complete = !state.clock_failed.load(Relaxed) && residual.is_some();
    Report {
        schema: 1,
        boundary: "index_command_excludes_cli_setup_teardown_and_report",
        source: source.filter(|s| {
            let sha = s.strip_suffix("-dirty").unwrap_or(s);
            matches!(sha.len(), 12 | 40) && sha.bytes().all(|b| b.is_ascii_hexdigit())
        }),
        invocation: crate::env::var("INVOCATION_ID")
            .ok()
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        returned_success: success,
        complete: success && !state.incomplete.load(Relaxed) && state.active.load(Relaxed) == 0 && cpu_complete,
        cpu_complete,
        process_cpu_ns,
        accounted_thread_cpu_ns,
        async_and_unassigned_cpu_ns: residual,
        metadata_input_bytes: None,
        non_jsonl_parse_bytes: None,
        physical_read_bytes: None,
        stages,
    }
}

/// CLI-owned report lifetime. Drop emits a partial report on unwinding. Termination without
/// unwinding cannot emit; this does not install a signal handler or change termination behavior.
/// Report write failure is ignored, never substituted for the command's own result.
pub struct Operation {
    state: Option<Arc<State>>,
    source: Option<&'static str>,
    _env: Option<crate::env::Scope>,
}

impl Operation {
    pub fn from_env(source: Option<&'static str>) -> Self {
        let enabled = crate::env::Env::current()
            .value_is(SETTING, "1")
            .unwrap_or(DEFAULT_ENABLED);
        let state = enabled.then(|| Arc::new(State::new()));
        let scope = state
            .as_ref()
            .map(|state| crate::env::with_stage_work(Context(Some(state.clone()))));
        Self {
            state,
            source,
            _env: scope,
        }
    }

    pub fn finish(mut self, success: bool) {
        self.emit(success);
    }

    fn emit(&mut self, success: bool) {
        use std::io::Write;
        if let Some(state) = self.state.take() {
            let report = report(&state, self.source, success);
            // Serialize only safe aggregates; use one locked stderr write without touching stdout.
            if let Ok(text) = serde_json::to_string(&report) {
                let _ = writeln!(std::io::stderr().lock(), "cv-stage-work {text}");
            }
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.emit(false);
    }
}

fn clock(thread: bool) -> Option<u64> {
    #[cfg(test)]
    CLOCK_READS.with(|count| count.set(count.get() + 1));
    platform_clock(thread)
}

#[cfg(unix)]
fn platform_clock(thread: bool) -> Option<u64> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    let id = if thread {
        libc::CLOCK_THREAD_CPUTIME_ID
    } else {
        libc::CLOCK_PROCESS_CPUTIME_ID
    };
    // SAFETY: clock_gettime initializes this timespec on success; it is read only then.
    if unsafe { libc::clock_gettime(id, time.as_mut_ptr()) } != 0 {
        return None;
    }
    let time = unsafe { time.assume_init() };
    u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)
}

#[cfg(not(unix))]
fn platform_clock(_thread: bool) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_has_no_state_and_nested_scopes_restore() {
        let _env = crate::env::Env::default().enter();
        let before = CLOCK_READS.with(std::cell::Cell::get);
        let check = |value: Option<std::ffi::OsString>| {
            let _env = crate::env::Env::from_vars(value.map(|v| (SETTING, v))).enter();
            let operation = Operation::from_env(None);
            assert!(operation.state.is_none());
            {
                let _scope = enter(Stage::Planning);
                add(Metric::Rows, 10);
                assert!(Context::current().0.is_none());
            }
            drop(operation);
        };
        for value in [None, Some("0"), Some("true"), Some("")] {
            check(value.map(Into::into));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            check(Some(std::ffi::OsString::from_vec(vec![0xff])));
        }
        assert_eq!(CLOCK_READS.with(std::cell::Cell::get), before);
    }

    #[test]
    fn explicit_one_enables_the_operation_and_reads_clocks() {
        let _env = crate::env::Env::from_vars([(SETTING, "1")]).enter();
        let before = CLOCK_READS.with(std::cell::Cell::get);
        let operation = Operation::from_env(None);
        assert!(operation.state.is_some() && enabled());
        assert!(CLOCK_READS.with(std::cell::Cell::get) > before);
        operation.finish(true);
        assert!(!enabled());
    }

    #[test]
    fn exclusive_partition_and_fixed_schema_do_not_retain_inputs() {
        let state = Arc::new(State::new());
        // Deterministic clock inputs verify exclusive accounting without a timing threshold.
        let thread = Thread {
            state: Some(state.clone()),
            frame: Some(Frame {
                stage: Stage::Planning,
                since: Some(10),
            }),
        };
        settle(&thread, Some(30));
        let nested = Thread {
            state: Some(state.clone()),
            frame: Some(Frame {
                stage: Stage::Resume,
                since: Some(30),
            }),
        };
        settle(&nested, Some(80));
        let restored = Thread {
            state: Some(state.clone()),
            frame: Some(Frame {
                stage: Stage::Planning,
                since: Some(80),
            }),
        };
        settle(&restored, Some(100));
        assert_eq!(state.stages[Stage::Planning as usize].cpu.load(Relaxed), 40);
        assert_eq!(state.stages[Stage::Resume as usize].cpu.load(Relaxed), 50);
        let encoded = serde_json::to_value(report(&state, Some("private/path"), false)).unwrap();
        assert!(encoded["source"].is_null());
        assert_eq!(encoded["complete"], false);
        assert!(encoded["physical_read_bytes"].is_null());
        assert_eq!(encoded["stages"].as_array().unwrap().len(), Stage::COUNT);
        for stage in encoded["stages"].as_array().unwrap() {
            assert_eq!(stage["work"].as_array().unwrap().len(), Metric::COUNT);
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn synthetic_snapshot_and_table_counters_use_existing_returned_bytes() {
        let world = cv_testkit::World::in_process("stage-work-sqlite", crate::env::Env::enter_isolated);
        let state = Arc::new(State::new());
        let _work = crate::env::with_stage_work(Context(Some(state.clone())));
        let sources = crate::source::read_scope();
        let database = world.base.join("synthetic.db");
        let writer = rusqlite::Connection::open(&database).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(7);")
            .unwrap();
        let database_bytes = std::fs::metadata(&database).unwrap().len();
        let wal_bytes = std::fs::metadata(database.with_file_name("synthetic.db-wal"))
            .unwrap()
            .len();
        assert!(wal_bytes > 0);
        for _ in 0..2 {
            let reader = crate::harness::store::sqlite::open_ro(&database).unwrap();
            assert_eq!(
                reader
                    .query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                7
            );
        }
        let amount = |stage: Stage, metric: Metric| state.stages[stage as usize].metrics[metric as usize].load(Relaxed);
        assert_eq!(amount(Stage::SqliteSnapshot, Metric::SnapshotMisses), 1);
        assert_eq!(amount(Stage::SqliteSnapshot, Metric::SnapshotHits), 1);
        assert_eq!(amount(Stage::SqliteSnapshot, Metric::DatabaseCopyBytes), database_bytes);
        assert_eq!(amount(Stage::SqliteSnapshot, Metric::WalCopyBytes), wal_bytes);
        assert_eq!(amount(Stage::SqliteSnapshot, Metric::JournalCopyBytes), 0);
        drop(sources);
        drop(writer);
        let _reader = crate::harness::store::sqlite::open_ro(&database).unwrap();
        assert_eq!(amount(Stage::SqliteSnapshot, Metric::ImmutableOpens), 1);

        #[derive(serde::Serialize, serde::Deserialize)]
        struct Payload;
        impl crate::source::Payload for Payload {
            const TAG: &'static str = "counts";
        }
        let record = crate::source::Record::new(
            crate::source::Baseline {
                primary: None,
                stamp: None,
                rows: None,
            },
            Payload,
        )
        .encode();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE source_dependencies(consumer TEXT,harness TEXT,id TEXT,path TEXT,record BLOB)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_dependencies VALUES('counts','synthetic','one','private',?1)",
            [&record],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_dependencies VALUES('counts','synthetic','bad','private',?1)",
            [b"invalid".as_slice()],
        )
        .unwrap();
        let _table = crate::source::Table::<Payload>::load_in(&conn, "counts");
        assert_eq!(amount(Stage::BaselineCounts, Metric::Rows), 2);
        assert_eq!(
            amount(Stage::BaselineCounts, Metric::InputBytes),
            record.len() as u64 + 7
        );
        assert_eq!(amount(Stage::BaselineCounts, Metric::DecodedRows), 1);
        let encoded = serde_json::to_string(&report(&state, None, true)).unwrap();
        assert!(!encoded.contains("private") && !encoded.contains("synthetic"));
    }

    #[test]
    fn panic_and_parallel_helpers_restore_context_without_cross_operation_leaks() {
        let _env = crate::env::Env::default().enter();
        let state = Arc::new(State::new());
        let _work = crate::env::with_stage_work(Context(Some(state.clone())));
        let _planning = enter(Stage::Planning);
        let _ = std::panic::catch_unwind(|| {
            let _nested = enter(Stage::Resume);
            panic!("synthetic unwind");
        });
        assert_eq!(current_stage(), Some(Stage::Planning));
        let rows = crate::par_filter_map((0..16).collect(), |item| {
            add(Metric::Candidates, 1);
            Some(item)
        });
        assert_eq!(rows.len(), 16);
        assert_eq!(
            state.stages[Stage::Planning as usize].metrics[Metric::Candidates as usize].load(Relaxed),
            16
        );
        let flat = crate::par_flat_map((0..16).collect(), |item| {
            add(Metric::Rows, 1);
            vec![item]
        });
        assert_eq!(flat.len(), 16);
        assert_eq!(
            state.stages[Stage::Planning as usize].metrics[Metric::Rows as usize].load(Relaxed),
            16
        );
        {
            let _other = crate::env::Env::default().enter();
            assert!(!enabled());
            add(Metric::Candidates, 100);
        }
        assert_eq!(current_stage(), Some(Stage::Planning));
        assert_eq!(
            state.stages[Stage::Planning as usize].metrics[Metric::Candidates as usize].load(Relaxed),
            16
        );
    }

    #[test]
    fn environment_and_source_workers_share_only_the_operation() {
        let _env = crate::env::Env::default().enter();
        let state = Arc::new(State::new());
        let _work = crate::env::with_stage_work(Context(Some(state.clone())));
        let sources = crate::source::read_scope();
        std::thread::scope(|threads| {
            threads.spawn(crate::env::carry(|| {
                sources.run(|| {
                    let _scope = enter(Stage::SelectedParse);
                    add(Metric::JsonlVisitedBytes, 17);
                })
            }));
        });
        assert_eq!(
            state.stages[Stage::SelectedParse as usize].metrics[Metric::JsonlVisitedBytes as usize].load(Relaxed),
            17
        );
        assert_eq!(state.active.load(Relaxed), 0);
        assert!(CURRENT.with(|slot| slot.borrow().frame.is_none()));
        incomplete();
        assert!(!report(&state, Some("abc123"), true).complete);
    }
}
