//! A persistent cache for the cheap metadata that `discover` produces.
//!
//! Discovery's expensive step is reading + JSON-parsing every transcript to extract metadata.
//! Exact cache hits require unchanged `(mtime, size)` and captured file identity, rejecting appends,
//! timestamp-preserving replacements and rewrites. A hit returns stored [`SessionRef`]s without
//! reading transcript content; full discovery still freshly enumerates each harness's inventory.
//!
//! It's process-global and shared by `cv` and the desktop app via the `scan_cache` table in
//! the shared catalog db (see [`crate::catalog`], which owns the DDL; the rows also feed the
//! freshness probe's append detection). Earlier releases kept a whole-loaded `discover.json`
//! instead — a second, redundant representation of the same facts that every command paid to parse;
//! [`load`] leaves a "safe to delete" note if one is still lying around. Reads are tolerant — a
//! missing or unreadable cache simply means everything is re-scanned (and the cache rebuilt).
//! Entries are reused only when interpretation, tuple, and captured file identity match.
//! Captured source identities also reject replacement or rewrites with restored timestamps.
//! Opted-in adapters may continue metadata on growth under their immutable-prefix contract,
//! using independent reducer state and sampled resume guards, never stale published refs.

use crate::ir::SessionRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

pub(crate) mod read;

enum Payload {
    #[cfg(feature = "sqlite")]
    Encoded { refs: String, reads: String },
    Parsed {
        refs: Vec<SessionRef>,
        reads: crate::source::ReadSet,
    },
    #[cfg(feature = "sqlite")]
    Invalid,
}

impl Payload {
    /// Decode only a demanded row. Invalid persisted metadata remains a cache miss; releasing
    /// encoded strings after either outcome avoids retaining both representations.
    #[cfg(feature = "sqlite")]
    fn parsed(&mut self) -> Option<(&mut Vec<SessionRef>, &crate::source::ReadSet)> {
        if let Self::Encoded { refs, reads } = self {
            *self = serde_json::from_str(refs)
                .and_then(|refs| serde_json::from_str(reads).map(|reads| Self::Parsed { refs, reads }))
                .unwrap_or(Self::Invalid);
        }
        match self {
            Self::Parsed { refs, reads } => Some((refs, reads)),
            _ => None,
        }
    }

    fn known(&self) -> Option<(&Vec<SessionRef>, &crate::source::ReadSet)> {
        match self {
            Self::Parsed { refs, reads } => Some((refs, reads)),
            #[cfg(feature = "sqlite")]
            _ => None,
        }
    }

    #[cfg(feature = "sqlite")]
    fn encoded(&self) -> bool {
        matches!(self, Self::Encoded { .. })
    }
}

/// A metadata reducer checkpoint, kept in scan_cache rather than consumer resume_marks.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ScanContinuation {
    pub head: crate::resume::MarkHead,
    pub checkpoint: crate::stream::Checkpoint,
    #[serde(default = "crate::source::ReadSet::unrecorded")]
    /// Auxiliary identities only: the growing primary is guarded by head and before/after reads.
    pub dependencies: crate::source::ReadSet,
}

struct Entry {
    #[cfg(feature = "sqlite")]
    mtime_ns: u64,
    size: u64,
    payload: Payload,
    /// Private reducer state, independent of mutable published refs; decode only on append.
    continuation: Option<String>,
    /// Scanned this run and not yet written back to sqlite.
    #[cfg(feature = "sqlite")]
    dirty: bool,
}

struct Cache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    any_dirty: AtomicBool,
    invalidated: AtomicBool,
}

/// Bump when discovery changes the interpretation of unchanged source bytes.
#[cfg(feature = "sqlite")]
pub(crate) const INTERPRETATION_VERSION: u32 = 4;
/// Every live cache by catalog. Each environment ([`crate::env`]) retains its current catalog's;
/// other homes survive only while operations use them.
static CACHE: OnceLock<RwLock<HashMap<PathBuf, std::sync::Weak<Cache>>>> = OnceLock::new();
fn cache_for(path: &Path) -> Arc<Cache> {
    let mut active = CACHE.get_or_init(Default::default).write().unwrap();
    active.retain(|_, cache| cache.strong_count() != 0);
    let cache = active.get(path).and_then(std::sync::Weak::upgrade).unwrap_or_else(|| {
        let cache = Arc::new(load());
        active.insert(path.to_path_buf(), Arc::downgrade(&cache));
        cache
    });
    *crate::env::Env::current().retained().lock().unwrap() = Some(Arc::new((path.to_path_buf(), cache.clone())));
    cache
}
#[cfg(all(test, feature = "sqlite"))]
fn current_cache() -> Arc<Cache> {
    read::Context::current().cache()
}
fn existing_cache(path: &Path) -> Option<Arc<Cache>> {
    let key = read::catalog_key(path);
    CACHE.get()?.read().ok()?.get(&key)?.upgrade()
}

#[cfg(feature = "sqlite")]
pub(crate) fn invalidate_interpretation(path: &Path) {
    if let Some(cache) = existing_cache(path) {
        cache.invalidated.store(true, Ordering::Release);
    }
}

fn clear_invalidated(cache: &Cache) {
    if cache.invalidated.load(Ordering::Acquire) {
        let mut entries = cache.entries.write().unwrap();
        if cache.invalidated.swap(false, Ordering::AcqRel) {
            entries.clear();
            cache.any_dirty.store(false, Ordering::Release);
        }
    }
}

/// One-time nudge about the pre-0.9.12 JSON cache this module replaced.
fn note_legacy_json() {
    let Some(path) = crate::env::cache_dir().map(|d| d.join("clustervision").join("discover.json")) else {
        return;
    };
    if path.exists() {
        eprintln!(
            "cv: note: stale discover.json no longer used (the scan cache lives in catalog.db now); safe to delete {}",
            path.display()
        );
    }
}

/// Start empty. Persisted payloads enter this existing cache only on exact keyed demand.
fn load() -> Cache {
    note_legacy_json();
    Cache {
        entries: Default::default(),
        any_dirty: AtomicBool::new(false),
        invalidated: AtomicBool::new(false),
    }
}

#[cfg(feature = "sqlite")]
fn load_entry(conn: &rusqlite::Connection, path: &Path, key: (u64, u64)) -> Option<Entry> {
    let mut statement = conn.prepare_cached("SELECT refs,source_reads,continuation FROM scan_cache WHERE path=?1 AND mtime_ns=?2 AND size=?3 AND interpretation_version=?4").ok()?;
    let (refs, reads, continuation) = statement
        .query_row(
            rusqlite::params![
                path.to_string_lossy(),
                key.0 as i64,
                key.1 as i64,
                INTERPRETATION_VERSION
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()?;
    Some(Entry {
        mtime_ns: key.0,
        size: key.1,
        payload: Payload::Encoded { refs, reads },
        continuation,
        dirty: false,
    })
}

#[cfg(feature = "sqlite")]
fn load_previous(conn: &rusqlite::Connection, path: &Path, size: u64) -> Option<(u64, String)> {
    conn.prepare_cached("SELECT size,continuation FROM scan_cache WHERE path=?1 AND size<?2 AND interpretation_version=?3 AND continuation IS NOT NULL").ok()?
        .query_row(rusqlite::params![path.to_string_lossy(),size as i64,INTERPRETATION_VERSION], |row| Ok((row.get::<_, i64>(0)? as u64,row.get(1)?))).ok()
}

#[cfg(feature = "sqlite")]
fn admit_loaded(
    cache: &Cache,
    path: &Path,
    entry: Entry,
    reads: &crate::source::ReadSet,
) -> Option<(Vec<SessionRef>, crate::source::ReadSet)> {
    let mut entries = cache.entries.write().unwrap();
    let entry = entries.entry(path.to_path_buf()).or_insert(entry);
    entry
        .payload
        .known()
        .and_then(|(refs, captured)| captured.includes(reads).then(|| (refs.clone(), captured.clone())))
}

/// The persisted key a capture of `path` would use now.
#[cfg(all(test, feature = "sqlite"))]
pub(crate) fn stat_key(path: &Path) -> Option<(u64, u64)> {
    let mut reads = crate::source::ReadSet::default();
    reads.record(path);
    reads.signature(path)
}

/// Return cached refs for `path` when its captured identity (and every auxiliary read) is
/// unchanged; otherwise run `scan`, cache any non-empty result, and return it. One file may map
/// to several sessions (e.g. Gemini logs).
///
/// The capture happens **before** the scan, so a write racing the scan leaves a too-old stamp in
/// the cache, which misses next time and re-scans. The safe direction.
pub(crate) fn cached_scan_many<F>(path: &Path, scan: F) -> Vec<SessionRef>
where
    F: FnOnce() -> Vec<SessionRef>,
{
    cached_scan_inner(path, None, Default::default(), |_| (scan(), None))
}

/// Single-result metadata scan with auxiliary identities captured before the caller reads them.
/// The existing primary cache retains and revalidates the complete baseline, including absence.
pub(crate) fn cached_scan_with_reads<F>(path: &Path, reads: crate::source::ReadSet, scan: F) -> Option<SessionRef>
where
    F: FnOnce() -> Option<SessionRef>,
{
    cached_scan_inner(path, None, reads, |_| (scan().into_iter().collect(), None))
        .into_iter()
        .next()
}

/// [`cached_scan_many`] with auxiliary identities captured before the caller reads them.
#[cfg(feature = "sqlite")]
pub(crate) fn cached_scan_many_with_reads<F>(path: &Path, reads: crate::source::ReadSet, scan: F) -> Vec<SessionRef>
where
    F: FnOnce() -> Vec<SessionRef>,
{
    cached_scan_inner(path, None, reads, |_| (scan(), None))
}

/// Opt-in append metadata state. Exact hits retain their existing guards; a continuation is
/// eligible only under the adapter's immutable-prefix append contract and sampled MarkHead.
pub(crate) fn cached_scan_continued<F>(path: &Path, version: &str, scan: F) -> Option<SessionRef>
where
    F: FnOnce(Option<&crate::stream::Checkpoint>) -> (Option<SessionRef>, Option<ScanContinuation>),
{
    cached_scan_inner(path, Some(version), Default::default(), |from| {
        let (reference, continuation) = scan(from);
        (reference.into_iter().collect(), continuation)
    })
    .into_iter()
    .next()
}

fn cached_scan_inner<F>(
    path: &Path,
    version: Option<&str>,
    mut reads: crate::source::ReadSet,
    scan: F,
) -> Vec<SessionRef>
where
    F: FnOnce(Option<&crate::stream::Checkpoint>) -> (Vec<SessionRef>, Option<ScanContinuation>),
{
    // One capture decides freshness: the path's full identity, inside the read set. Its
    // `(mtime, size)` only keys the persisted rows.
    reads.record(path);
    #[cfg_attr(not(feature = "sqlite"), allow(unused_variables))]
    let Some(key @ (_, size)) = reads.signature(path) else {
        return scan(None).0;
    };
    let context = read::Context::current();
    let cache = context.cache();
    clear_invalidated(&cache);
    let candidate = {
        let entries = cache.entries.read().unwrap();
        entries
            .get(path)
            .and_then(|entry| entry.payload.known())
            .and_then(|(refs, captured)| captured.includes(&reads).then(|| (refs.clone(), captured.clone())))
    };
    if let Some((refs, captured)) = candidate {
        if captured.fresh(&mut crate::source::Probe::default()) {
            return refs;
        }
    }
    // Do not reload an existing process entry, even when stale or invalid: a fresh scan replaces it.
    #[cfg(feature = "sqlite")]
    if !cache.entries.read().unwrap().contains_key(path) {
        #[cfg(test)]
        context.note_lookup();
        let loaded = context.with_connection(|conn| load_entry(conn, path, key)).flatten();
        // A lazy standalone open may migrate interpretation; clear before retaining this scan.
        clear_invalidated(&cache);
        if let Some(mut entry) = loaded {
            #[cfg(test)]
            context.note_row();
            // Decode outside locks, then choose the actual current process entry. A concurrent
            // scan/count update must win over the older SQL snapshot, including on return.
            entry.payload.parsed();
            let candidate = admit_loaded(&cache, path, entry, &reads);
            if let Some((refs, captured)) = candidate {
                if captured.fresh(&mut crate::source::Probe::default()) {
                    return refs;
                }
            }
        }
    }
    #[allow(unused_mut)] // Persistence may replace the in-process candidate with a demanded row.
    let mut previous = version.and_then(|_| {
        let entries = cache.entries.read().unwrap();
        entries
            .get(path)
            .and_then(|entry| entry.continuation.as_ref().map(|state| (entry.size, state.clone())))
    });
    #[cfg(feature = "sqlite")]
    if version.is_some() && !cache.entries.read().unwrap().contains_key(path) {
        #[cfg(test)]
        context.note_lookup();
        previous = context
            .with_connection(|conn| load_previous(conn, path, size))
            .flatten();
        // As with exact demand, a newer in-process row wins over an older SQL snapshot.
        if let Some(entry) = cache.entries.read().unwrap().get(path) {
            previous = entry.continuation.as_ref().map(|state| (entry.size, state.clone()));
        }
    }
    let prior = previous.and_then(|(old_size, text)| {
        if old_size >= size {
            return None;
        }
        let prior: ScanContinuation = serde_json::from_str(&text).ok()?;
        (prior.head.offset <= old_size
            && prior.checkpoint.offset == prior.head.offset
            && prior.dependencies.fresh(&mut crate::source::Probe::default())
            && prior.head.holds(path, size, version?))
        .then_some(prior)
    });
    if let Some(prior) = &prior {
        reads.merge(&prior.dependencies);
    }
    let (refs, mut continuation) = scan(prior.as_ref().map(|prior| &prior.checkpoint));
    if let Some(state) = &mut continuation {
        if let Some(prior) = &prior {
            state.dependencies.merge(&prior.dependencies);
        }
        reads.merge(&state.dependencies);
    }
    let continuation = continuation
        .filter(|state| {
            Some(state.head.version.as_str()) == version
                && state.head.offset == state.checkpoint.offset
                && state.head.offset <= size
                && reads.fresh(&mut crate::source::Probe::default())
                && state.dependencies.fresh(&mut crate::source::Probe::default())
        })
        .and_then(|state| serde_json::to_string(&state).ok());
    // Only cache hits — empty results are cheap to recompute (a failed name/content check that never
    // read the file) and caching them would bloat the table with every non-transcript path.
    if !refs.is_empty() {
        // Keep filesystem probes outside the cache lock. A raced pass cannot publish an
        // authoritative append state or displace a newer row; its caller still gets its result.
        if version.is_some() && !reads.fresh(&mut crate::source::Probe::default()) {
            return refs;
        }
        let mut entries = cache.entries.write().unwrap();
        entries.insert(
            path.to_path_buf(),
            Entry {
                #[cfg(feature = "sqlite")]
                mtime_ns: key.0,
                size,
                payload: Payload::Parsed {
                    refs: refs.clone(),
                    reads,
                },
                continuation,
                #[cfg(feature = "sqlite")]
                dirty: true,
            },
        );
        cache.any_dirty.store(true, Ordering::Relaxed);
    }
    refs
}

/// Original before-read metadata, never a fresh post-discovery stamp.
#[cfg(feature = "sqlite")]
pub(crate) fn captured_reads(path: &Path) -> Option<crate::source::ReadSet> {
    let cache = read::Context::current().cached()?;
    {
        let entries = cache.entries.read().ok()?;
        let e = entries.get(path)?;
        if let Some((_, reads)) = e.payload.known() {
            return Some(reads.clone());
        }
        if !e.payload.encoded() {
            return None;
        }
    }
    let mut entries = cache.entries.write().ok()?;
    let (_, reads) = entries.get_mut(path)?.payload.parsed()?;
    Some(reads.clone())
}

/// Single-result convenience over [`cached_scan_many`].
pub(crate) fn cached_scan<F>(path: &Path, scan: F) -> Option<SessionRef>
where
    F: FnOnce() -> Option<SessionRef>,
{
    cached_scan_many(path, || scan().into_iter().collect())
        .into_iter()
        .next()
}

fn persistence_cache(path: &Path, prune: bool) -> Option<Arc<Cache>> {
    let cache = existing_cache(path)?;
    (cache.any_dirty.load(Ordering::Relaxed) || prune).then_some(cache)
}

/// Flush freshly-scanned entries to the catalog db in one transaction. With `prune`, also drop
/// rows whose files no longer exist so the table doesn't grow without bound — pass it after a
/// *full* [`crate::discover_all`] (the stat-per-row cost belongs on the slow path; per-harness
/// probe refreshes skip it). Best-effort: a failed flush just means a re-scan next run.
pub(crate) fn persist(prune: bool) {
    if persistence_cache(&read::Context::current().path(), prune).is_some() {
        #[cfg(feature = "sqlite")]
        read::Context::current().with_connection_mut(|conn| persist_in(conn, prune));
    }
}

#[cfg(feature = "sqlite")]
pub(crate) fn persist_in(conn: &mut rusqlite::Connection, prune: bool) {
    let Some(cache) = persistence_cache(Path::new(conn.path().unwrap_or("")), prune) else {
        return;
    };
    clear_invalidated(&cache);
    // Full discovery still checks every persisted path, including deferred or malformed
    // payloads. Pruning therefore keeps its existing completeness semantics.
    let mut prune_paths: std::collections::HashSet<String> = if prune {
        let Ok(mut stmt) = conn.prepare("SELECT path FROM scan_cache") else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            return;
        };
        let Ok(paths) = rows.collect::<rusqlite::Result<_>>() else {
            return;
        };
        paths
    } else {
        Default::default()
    };
    let mut g = cache.entries.write().unwrap();
    if prune {
        // Match the previous flush-then-prune behavior for newly scanned files that
        // vanished before publication, checking each persisted/dirty path only once.
        prune_paths.extend(
            g.iter()
                .filter(|(_, e)| e.dirty)
                .map(|(path, _)| path.to_string_lossy().into_owned()),
        );
    }
    let vanished: Vec<_> = prune_paths.into_iter().filter(|p| !Path::new(p).exists()).collect();
    if vanished.is_empty() && !g.values().any(|entry| entry.dirty) {
        return;
    }
    let Ok(tx) = conn.transaction() else { return };
    let mut written = Vec::new();
    {
        let Ok(mut stmt) = tx.prepare("INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs,interpretation_version,source_reads,continuation) VALUES(?1,?2,?3,?4,?5,?6,?7)") else { return };
        for (path, e) in g.iter().filter(|(_, e)| e.dirty) {
            let Some((refs, reads)) = e.payload.known() else {
                continue;
            };
            let Ok(refs) = serde_json::to_string(refs) else {
                continue;
            };
            let Ok(reads) = serde_json::to_string(reads) else {
                continue;
            };
            if stmt
                .execute(rusqlite::params![
                    path.to_string_lossy(),
                    e.mtime_ns as i64,
                    e.size as i64,
                    refs,
                    INTERPRETATION_VERSION,
                    reads,
                    e.continuation
                ])
                .is_err()
            {
                return;
            }
            written.push(path.clone());
        }
    }
    for path in &vanished {
        if tx.execute("DELETE FROM scan_cache WHERE path=?1", [path]).is_err() {
            return;
        }
    }
    if tx.commit().is_err() {
        return;
    }
    // A failed transaction retains dirty flags for retry rather than losing its writes.
    for path in written {
        if let Some(entry) = g.get_mut(&path) {
            entry.dirty = false;
        }
    }
    for path in vanished {
        g.remove(Path::new(&path));
    }
    cache
        .any_dirty
        .store(g.values().any(|entry| entry.dirty), Ordering::Relaxed);
}

/// [`crate::catalog::fill_count`]'s scan-cache half: set `message_count = n` on the cached ref for
/// `(path, harness, id)` where it is `0`, both in the persisted row (left at its recorded
/// `(mtime, size)`, so a changed file still misses and re-scans) and in this process's copy.
#[cfg(feature = "sqlite")]
pub(crate) fn fill_count(conn: &rusqlite::Connection, path: &Path, harness: crate::ir::Harness, id: &str, n: usize) {
    let fill = |refs: &mut [SessionRef]| {
        let mut hit = false;
        for r in refs
            .iter_mut()
            .filter(|r| r.harness == harness && r.id == id && r.message_count == 0)
        {
            r.message_count = n;
            hit = true;
        }
        hit
    };
    if let Some(cache) = existing_cache(Path::new(conn.path().unwrap_or(""))) {
        clear_invalidated(&cache);
        if let Some(e) = cache.entries.write().unwrap().get_mut(path) {
            if let Some((refs, _)) = e.payload.parsed() {
                fill(refs);
            }
        }
    }
    let key = path.to_string_lossy();
    let Ok(json) = conn.query_row(
        "SELECT refs FROM scan_cache WHERE path=?1 AND interpretation_version=?2",
        rusqlite::params![key, INTERPRETATION_VERSION],
        |row| row.get::<_, String>(0),
    ) else {
        return;
    };
    let Ok(mut refs) = serde_json::from_str::<Vec<SessionRef>>(&json) else {
        return;
    };
    if fill(&mut refs) {
        if let Ok(json) = serde_json::to_string(&refs) {
            let _ = conn.execute(
                "UPDATE scan_cache SET refs=?1 WHERE path=?2 AND interpretation_version=?3",
                rusqlite::params![json, key, INTERPRETATION_VERSION],
            );
        }
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    #[test]
    fn auxiliary_scan_baselines_invalidate_changes_and_preserve_raced_stamps() {
        let world = crate::test_world("scan-auxiliary-baselines");
        let path = world.base.join("source");
        let sidecar = world.base.join("state");
        std::fs::write(&path, "unchanged primary").unwrap();
        let baseline = || {
            let mut reads = crate::source::ReadSet::default();
            reads.record(&sidecar);
            reads
        };
        let reference = |id| SessionRef::new(id, crate::Harness::Claude, &path);
        assert_eq!(
            cached_scan_with_reads(&path, baseline(), || Some(reference("absent")))
                .unwrap()
                .id,
            "absent"
        );
        cached_scan_with_reads(&path, baseline(), || panic!("unchanged absent sidecar must hit"));
        persist(false);
        current_cache().entries.write().unwrap().clear();
        cached_scan_with_reads(&path, baseline(), || panic!("persisted auxiliary baseline must hit"));
        std::fs::write(&sidecar, "new title").unwrap();
        assert_eq!(
            cached_scan_with_reads(&path, baseline(), || Some(reference("present")))
                .unwrap()
                .id,
            "present"
        );
        std::fs::write(&sidecar, "before race").unwrap();
        cached_scan_with_reads(&path, baseline(), || {
            std::fs::write(&sidecar, "after race").unwrap();
            Some(reference("raced"))
        });
        assert!(!captured_reads(&path)
            .unwrap()
            .fresh(&mut crate::source::Probe::default()));
        assert_eq!(
            cached_scan_with_reads(&path, baseline(), || Some(reference("refreshed")))
                .unwrap()
                .id,
            "refreshed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn continuation_checks_auxiliary_identity_and_does_not_publish_raced_passes() {
        let w = crate::test_world("scan-continuation-freshness");
        let path = w.base.join("s.jsonl");
        let aux = w.base.join("dependency");
        std::fs::write(&path, "first\n").unwrap();
        std::fs::write(&aux, "one").unwrap();
        let version = "test-discovery/1";
        let r = SessionRef::new("s", crate::Harness::Claude, &path);
        let state = || {
            let cp = crate::stream::Checkpoint::new(std::fs::metadata(&path).unwrap().len());
            let mut dependencies = crate::source::ReadSet::default();
            dependencies.record(&aux);
            ScanContinuation {
                head: crate::resume::MarkHead::of(&path, &cp, version).unwrap(),
                checkpoint: cp,
                dependencies,
            }
        };
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_none());
            (Some(r.clone()), Some(state()))
        });
        cached_scan_continued(&path, version, |_| {
            panic!("unchanged primary and dependency must hit process cache")
        });
        persist(false);
        current_cache().entries.write().unwrap().clear();
        cached_scan_continued(&path, version, |_| {
            panic!("persisted exact hit must retain auxiliary guards")
        });
        // A dependency-only change forces complete metadata discovery, even with identical
        // primary bytes and no opportunity for append continuation.
        std::fs::write(&aux, "replacement auxiliary").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_none());
            (Some(r.clone()), Some(state()))
        });
        cached_scan_continued(&path, version, |_| panic!("refreshed auxiliary must hit"));
        std::fs::write(&path, "first\nsecond\n").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert_eq!(prior.unwrap().offset, 6);
            (Some(r.clone()), Some(state()))
        });
        std::fs::write(&aux, "two").unwrap();
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_none(), "changed auxiliary rejects append authority");
            (Some(r.clone()), Some(state()))
        });
        let cache = current_cache();
        let old_size = cache.entries.read().unwrap()[&path].size;
        std::fs::write(&path, "first\nsecond\nthird\nfourth\n").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_some());
            let continuation = state();
            std::fs::write(&aux, "new").unwrap();
            (Some(r.clone()), Some(continuation))
        });
        assert_eq!(
            cache.entries.read().unwrap()[&path].size,
            old_size,
            "raced dependency cannot bless a new cache tuple"
        );
        let result = cached_scan_continued(&path, version, |_| {
            std::fs::write(&path, "first\nsecond\nthird\nfourth\nfifth\n").unwrap();
            let newer = cached_scan_continued(&path, version, |_| {
                (
                    Some(SessionRef {
                        message_count: 9,
                        ..r.clone()
                    }),
                    Some(state()),
                )
            })
            .unwrap();
            assert_eq!(newer.message_count, 9);
            (
                Some(SessionRef {
                    message_count: 1,
                    ..r.clone()
                }),
                Some(state()),
            )
        })
        .unwrap();
        assert_eq!(result.message_count, 1);
        assert_eq!(
            cache.entries.read().unwrap()[&path].payload.known().unwrap().0[0].message_count,
            9,
            "older raced primary pass cannot replace a newer installed row"
        );
    }

    #[test]
    fn lazy_payload_preserves_demand_guards_count_updates_and_malformed_fallback() {
        let w = crate::test_world("scan-lazy");
        let mut conn = crate::catalog::open_db().unwrap();
        let paths: Vec<_> = ["valid", "invalid", "stale", "invalid-reads"]
            .map(|name| w.base.join(name))
            .into();
        let mut expected = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            std::fs::write(path, "content").unwrap();
            let r = SessionRef {
                harness: crate::Harness::Claude,
                id: format!("row-{i}"),
                path: path.clone(),
                cwd: None,
                title: Some("metadata".into()),
                created_at: None,
                updated_at: None,
                message_count: 0,
            };
            let mut reads = crate::source::ReadSet::default();
            reads.record(path);
            let (mtime, size) = stat_key(path).unwrap();
            conn.execute(
                "INSERT INTO scan_cache(path,mtime_ns,size,refs,source_reads,interpretation_version) VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![path.to_string_lossy(), mtime as i64, size as i64,
                    if i == 0 || i == 3 { serde_json::to_string(std::slice::from_ref(&r)).unwrap() } else { "invalid".into() },
                    if i == 3 { "invalid".into() } else { serde_json::to_string(&reads).unwrap() }, INTERPRETATION_VERSION],
            ).unwrap();
            expected.push((r, reads));
        }
        let cache = current_cache();
        clear_invalidated(&cache);
        assert!(
            cache.entries.read().unwrap().is_empty(),
            "no unrelated persisted payload is loaded"
        );
        // Count publication may demand an encoded row before discovery does.
        fill_count(&conn, &paths[0], crate::Harness::Claude, "row-0", 2);
        assert_eq!(
            cached_scan(&paths[0], || panic!("valid persisted row must hit"))
                .unwrap()
                .message_count,
            2
        );
        assert_eq!(captured_reads(&paths[0]).unwrap(), expected[0].1);
        assert!(
            !cache.entries.read().unwrap().contains_key(&paths[1]),
            "unrelated malformed row stays unloaded"
        );
        assert!(captured_reads(&paths[1]).is_none());
        assert!(cached_scan_many(&paths[1], Vec::new).is_empty());
        assert!(matches!(
            cache.entries.read().unwrap()[&paths[1]].payload,
            Payload::Invalid
        ));
        let repaired = cached_scan(&paths[1], || Some(expected[1].0.clone())).unwrap();
        assert_eq!(repaired.id, expected[1].0.id);
        std::fs::write(&paths[2], "appended content").unwrap();
        let fresh = cached_scan(&paths[2], || {
            assert!(
                !cache.entries.read().unwrap().contains_key(&paths[2]),
                "tuple mismatch avoids persisted payload loading"
            );
            Some(SessionRef {
                message_count: 3,
                ..expected[2].0.clone()
            })
        })
        .unwrap();
        assert_eq!(fresh.message_count, 3);
        assert!(cached_scan_many(&paths[3], Vec::new).is_empty());
        assert!(captured_reads(&paths[3]).is_none(), "invalid ReadSet must also miss");
        assert_eq!(
            cached_scan(&paths[3], || Some(expected[3].0.clone())).unwrap().id,
            expected[3].0.id
        );
        persist_in(&mut conn, false);
        for (i, path) in paths.iter().enumerate() {
            let mut entry = load_entry(&conn, path, stat_key(path).unwrap()).unwrap();
            let (refs, reads) = entry.payload.parsed().unwrap();
            assert_eq!(refs[0].message_count, [2, 0, 3, 0][i]);
            assert!(reads.fresh(&mut crate::source::Probe::default()));
        }
        std::fs::remove_file(&paths[1]).unwrap();
        persist_in(&mut conn, true);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1",
                [paths[1].to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn interpretation_upgrade_rejects_persisted_and_process_scan_rows() {
        let w = crate::test_world("scan-interpretation");
        let path = w.base.join("unchanged.jsonl");
        std::fs::write(&path, "unchanged").unwrap();
        let old = SessionRef {
            harness: crate::Harness::Codex,
            id: "old".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 4,
        };
        assert_eq!(cached_scan(&path, || Some(old.clone())).unwrap().id, "old");
        persist(false);
        let conn = crate::catalog::open_db().unwrap();
        conn.execute(
            "UPDATE scan_cache SET interpretation_version=?1",
            [INTERPRETATION_VERSION - 1],
        )
        .unwrap();
        conn.execute(
            "UPDATE catalog_meta SET value=?1 WHERE key='discovery_version'",
            [(INTERPRETATION_VERSION - 1).to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync','1');
            INSERT OR REPLACE INTO watched(harness,path,mtime_ns) VALUES('gemini','old-watch',1);",
        )
        .unwrap();
        let conn = crate::catalog::open_db().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM watched", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(crate::catalog::last_full_sync().is_none());
        // The first full discovery after the upgrade announces itself, once.
        let reader = crate::catalog::Reader::open().unwrap();
        assert!(reader.take_rediscovery_notice());
        assert!(!reader.take_rediscovery_notice());
        assert!(
            load_entry(&conn, &path, stat_key(&path).unwrap()).is_none(),
            "old persisted interpretation ignored"
        );
        let current = SessionRef {
            id: "current".into(),
            message_count: 2,
            ..old
        };
        assert_eq!(
            cached_scan(&path, || Some(current)).unwrap().id,
            "current",
            "process entry invalidated despite unchanged bytes"
        );
        assert_eq!(
            cached_scan(&path, || panic!("current scan must stay cached"))
                .unwrap()
                .message_count,
            2
        );
        persist(false);
        assert_eq!(
            conn.query_row(
                "SELECT interpretation_version FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
            INTERPRETATION_VERSION
        );
        // Old binaries omit the added nullable field on replacement: their row remains stale.
        conn.execute(
            "INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs) VALUES(?1,1,1,'[]')",
            [path.to_string_lossy()],
        )
        .unwrap();
        assert!(load_entry(&conn, &path, stat_key(&path).unwrap()).is_none());
    }
    #[test]
    fn concurrent_count_update_wins_over_loaded_sql_snapshot() {
        let world = crate::test_world("scan-admission-count");
        let path = world.base.join("source");
        std::fs::write(&path, "metadata").unwrap();
        let reference = SessionRef::new("counted", crate::Harness::Claude, path.clone());
        cached_scan(&path, || Some(reference));
        persist(false);
        let conn = crate::catalog::open_db().unwrap();
        let key = stat_key(&path).unwrap();
        let mut old_sql = load_entry(&conn, &path, key).unwrap();
        assert_eq!(old_sql.payload.parsed().unwrap().0[0].message_count, 0);
        fill_count(&conn, &path, crate::Harness::Claude, "counted", 7);
        let cache = current_cache();
        let mut reads = crate::source::ReadSet::default();
        reads.record(&path);
        let actual = admit_loaded(&cache, &path, old_sql, &reads).unwrap();
        assert_eq!(
            actual.0[0].message_count, 7,
            "newer process count wins on admission AND return"
        );
    }

    #[test]
    fn failed_flush_retries_and_prunes_newly_vanished_entries() {
        let w = crate::test_world("scan-persist-retry");
        let path = w.base.join("retry.jsonl");
        std::fs::write(&path, "data").unwrap();
        let r = SessionRef {
            path: path.clone(),
            id: "retry".into(),
            harness: crate::Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        let conn = crate::catalog::open_db().unwrap();
        cached_scan(&path, || Some(r.clone()));
        conn.execute_batch(
            "CREATE TRIGGER reject_cache BEFORE INSERT ON scan_cache BEGIN SELECT RAISE(ABORT,'retry'); END;",
        )
        .unwrap();
        persist(false);
        assert!(current_cache().entries.read().unwrap().get(&path).unwrap().dirty);
        conn.execute_batch("DROP TRIGGER reject_cache;").unwrap();
        persist(false);
        assert!(!current_cache().entries.read().unwrap().get(&path).unwrap().dirty);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let vanished = w.base.join("newly-vanished.jsonl");
        std::fs::write(&vanished, "data").unwrap();
        cached_scan(&vanished, || {
            Some(SessionRef {
                path: vanished.clone(),
                ..r
            })
        });
        std::fs::remove_file(&vanished).unwrap();
        // Include a malformed persisted row: pruning must not rely only on loaded entries.
        conn.execute("INSERT INTO scan_cache(path,mtime_ns,size,refs,source_reads) VALUES('missing-malformed',1,1,'broken','broken')", []).unwrap();
        persist(true);
        assert!(!current_cache().entries.read().unwrap().contains_key(&vanished));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1 OR path='missing-malformed'",
                [vanished.to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn discovery_mutation_keeps_original_identity_and_forces_rescan() {
        let w = crate::test_world("scan-mutation");
        let path = w.base.join("raced.jsonl");
        std::fs::write(&path, "before").unwrap();
        let r = SessionRef {
            path: path.clone(),
            id: "old".into(),
            harness: crate::Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        cached_scan(&path, || {
            std::fs::write(&path, "after!").unwrap();
            Some(r.clone())
        })
        .unwrap();
        let reads = captured_reads(&path).unwrap();
        crate::catalog::replace_harness(
            crate::Harness::Claude,
            std::slice::from_ref(&r),
            &[],
            &Default::default(),
        );
        let conn = crate::catalog::open_db().unwrap();
        let json: String = conn
            .query_row(
                "SELECT reads FROM primary_sources WHERE path=?1",
                [path.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        let published: crate::source::ReadSet = serde_json::from_str(&json).unwrap();
        assert_eq!(published, reads, "catalog must publish the original captured identity");
        assert!(
            !reads.fresh(&mut crate::source::Probe::default()),
            "publication must retain the before-read identity"
        );
        let fresh = cached_scan(&path, || Some(SessionRef { id: "new".into(), ..r })).unwrap();
        assert_eq!(fresh.id, "new");
        persist(false);
        let mut loaded = load_entry(&conn, &path, stat_key(&path).unwrap()).unwrap();
        assert!(loaded
            .payload
            .parsed()
            .unwrap()
            .1
            .fresh(&mut crate::source::Probe::default()));
    }
}
