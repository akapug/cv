//! Full-text search via tantivy.
//!
//! A session is one **meta** doc plus its **body** docs (`kind`), all sharing its `id`. The meta doc
//! owns what describes the session as a whole — the freshness signature, the label, the dates, the
//! preview — and is rewritten on every pass; the body docs hold the searchable text in chunks, each
//! stamped with the index of its first message (`first_msg`), so a pass that resumes at message
//! `n` of a grown transcript replaces only the body docs from `n` on (see [`sweep`]).
//!
//! Schema fields:
//! - `id`         STRING | STORED  — exact session id (the delete key on reindex)
//! - `kind`       STRING | STORED  — `meta` or `body`
//! - `harness`    STRING | STORED  — fielded filter, e.g. `harness:claude`
//! - `cwd`        TEXT   | STORED  — tokenized so path fragments are searchable
//! - `title`      TEXT   | STORED  — on the meta doc the current label; on a body doc the label
//!   when it was written (so a title term still matches a conjunctive query on that chunk)
//! - `body`       TEXT             — the big searchable blob, body docs only; **indexed but not stored**
//! - `first_msg`  i64 INDEXED | STORED — body docs: the index of the first message they hold
//! - `path`       STRING | STORED  — source file, so a hit can be snippeted by re-reading just
//!   that one session live (no full stored body — keeps the index lean)
//! - `preview`    STORED (only)    — meta doc: ~240-char whitespace-collapsed head of the body
//!   (~240 bytes/session), the snippet fallback when the source file has moved/changed
//! - `created_at`/`updated_at` i64 INDEXED | STORED — returned on hits + future sort/filter;
//!   `updated_at` only on the meta doc (it moves with every append, and a resumed pass keeps the
//!   body docs it does not replace)
//! - `mtime`      i64 STORED       — meta doc: file mtime, still used for non-append transcript stores
//! - `discovery_metadata` STORED — discovery inputs and provenance guarding skip/resume;
//!   changed body facets require a whole rewrite because body text is not stored.
//! - `size`       i64 STORED       — meta doc: file size, the primary incremental-index key for
//!   append-only session logs. Skip also requires the captured primary identity and auxiliary
//!   source guards; a changed tuple or identity requires a pass.
//!
//! Indexing is **incremental** by default: only sessions whose file signature changed (plus new ones)
//! are (re)written, and sessions whose files vanished are deleted. A full rebuild is `rebuild=true`.
//! Snippets are generated **live** from the top hits (re-reading the session, capped) rather than
//! from a stored copy of every body — which is what keeps the on-disk index small.

use crate::ingest::{Admission, AdmittedDocument, Writer};
use crate::Hit;
use anyhow::{Context, Result};
use cv_core::scan::{ceil_char, floor_char};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, TantivyDocument, Term};

#[derive(Clone)]
struct Fields {
    id: Field,
    kind: Field,
    first_msg: Field,
    harness: Field,
    cwd: Field,
    title: Field,
    body: Field,
    path: Field,
    preview: Field,
    created_at: Field,
    updated_at: Field,
    mtime: Field,
    size: Field,
    discovery_metadata: Field,
    /// Sub-agent provenance, written only on folded-in sub-agent docs (`cv index --subagents`):
    /// `parent_id` is fielded (`parent_id:<top-level id>` filters to one parent's whole forest),
    /// `agent_id`/`workflow` are stored-only attribution carried onto the hit. NULL/absent on
    /// top-level docs, which keeps those docs byte-for-byte as before.
    parent_id: Field,
    agent_id: Field,
    workflow: Field,
}

fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("id", STRING | STORED);
    b.add_text_field("kind", STRING | STORED);
    b.add_i64_field("first_msg", INDEXED | STORED);
    b.add_text_field("harness", STRING | STORED);
    b.add_text_field("cwd", TEXT | STORED);
    b.add_text_field("title", TEXT | STORED);
    b.add_text_field("body", TEXT);
    b.add_text_field("path", STRING | STORED);
    b.add_text_field("preview", STORED); // stored-only: never searched, just the snippet fallback
    b.add_i64_field("created_at", INDEXED | STORED);
    b.add_i64_field("updated_at", INDEXED | STORED);
    b.add_i64_field("mtime", STORED);
    b.add_text_field("discovery_metadata", STORED); // append reuse guard, not searched
    b.add_i64_field("size", STORED); // primary incremental key for append-only logs
    b.add_text_field("parent_id", STRING | STORED); // fielded: `parent_id:<id>` scopes to a forest
    b.add_text_field("agent_id", STRING | STORED); // stored attribution
    b.add_text_field("workflow", STRING | STORED); // stored attribution
    b.build()
}

fn fields_of(schema: &Schema) -> Result<Fields> {
    let get = |name: &str| schema.get_field(name);
    Ok(Fields {
        id: get("id")?,
        kind: get("kind")?,
        first_msg: get("first_msg")?,
        harness: get("harness")?,
        cwd: get("cwd")?,
        title: get("title")?,
        body: get("body")?,
        path: get("path")?,
        preview: get("preview")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
        mtime: get("mtime")?,
        size: get("size")?,
        discovery_metadata: get("discovery_metadata")?,
        parent_id: get("parent_id")?,
        agent_id: get("agent_id")?,
        workflow: get("workflow")?,
    })
}

/// Whether an on-disk index carries every field of the current schema. A miss means it was built
/// by an older `cv` (pre-`path`/`mtime`/`preview`, pre-subagent-provenance, pre-`size`/discovery-metadata guard, or one doc
/// kind for everything) and must be rebuilt fresh — one-time on upgrade, from the indexing path only.
fn schema_current(schema: &Schema) -> bool {
    [
        "kind",
        "first_msg",
        "path",
        "mtime",
        "preview",
        "parent_id",
        "agent_id",
        "workflow",
        "size",
        "discovery_metadata",
    ]
    .iter()
    .all(|f| schema.get_field(f).is_ok())
}

/// Open the index at `dir` for **indexing**: created when absent, and an existing index with a
/// stale schema (see [`schema_current`]) is transparently rebuilt fresh so the binary self-heals
/// on upgrade — the caller's sweep repopulates it. Any *other* open failure (corruption, a
/// transient IO/lock error) propagates: only a positively-identified schema mismatch may delete;
/// read-only callers use [`open_existing`], which never deletes anything.
fn open_or_create(dir: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating tantivy dir {}", dir.display()))?;
    // No `meta.json` → nothing here is an index (a fresh dir, or stray leftovers from a failed
    // create): safe to start clean.
    if !dir.join("meta.json").exists() {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).with_context(|| format!("recreating tantivy dir {}", dir.display()))?;
        let index = Index::create_in_dir(dir, build_schema())
            .with_context(|| format!("creating tantivy index in {}", dir.display()))?;
        let fields = fields_of(&index.schema())?;
        return Ok((index, fields));
    }
    let index = Index::open_in_dir(dir).with_context(|| {
        format!(
            "opening tantivy index at {} (if it is corrupt, delete the directory and re-run `cv index`)",
            dir.display()
        )
    })?;
    if schema_current(&index.schema()) {
        let fields = fields_of(&index.schema())?;
        return Ok((index, fields));
    }
    // Positively identified as an older-schema index → rebuild fresh.
    std::fs::remove_dir_all(dir).with_context(|| format!("clearing stale-schema index at {}", dir.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("recreating tantivy dir {}", dir.display()))?;
    let index = Index::create_in_dir(dir, build_schema())
        .with_context(|| format!("creating tantivy index in {}", dir.display()))?;
    let fields = fields_of(&index.schema())?;
    Ok((index, fields))
}

/// Open the index at `dir` for **querying**: never creates and never deletes — a transient open
/// failure during a search must not wipe an index that took an hour to build. A stale-schema index
/// is an error pointing at `cv index` (whose path is the one allowed to rebuild).
fn open_existing(dir: &Path) -> Result<(Index, Fields)> {
    let index = Index::open_in_dir(dir).with_context(|| format!("opening tantivy index at {}", dir.display()))?;
    if !schema_current(&index.schema()) {
        anyhow::bail!(
            "full-text index at {} was built by an older cv — run `cv index` to rebuild it",
            dir.display()
        );
    }
    let fields = fields_of(&index.schema())?;
    Ok((index, fields))
}

const COMMIT_EVERY: usize = 256;

/// A segment at most this many bytes on disk sits in the bottom merge tier; each tier above holds
/// segments [`TIER_FANOUT`] times larger than the one below.
const TIER_FLOOR: u64 = 8 << 20;

/// How many segments a tier holds before [`compact`] merges them into one.
const TIER_FANOUT: usize = 8;

/// A segment whose deleted-doc share exceeds this is rewritten alone, dropping the dead docs (an
/// actively-written session is re-indexed on every run, so its old docs pile up as tombstones).
const DEAD_RATIO: f32 = 0.5;

/// A writer that never merges on its own. tantivy's default `LogMergePolicy` tiers segments by doc
/// count with a 10,000-doc floor, and this index has ~31k docs in all (a doc is a 4 MB chunk), so
/// every segment but the largest shares one tier: every commit started a background merge of
/// ~1.2 GB that ran ~11 s at ~1 GB resident — and `cv index` exited first, abandoning it, so the
/// next run started it again. Merging is [`compact`]'s job, sized by bytes, run to completion.
fn open_writer(index: &Index, heap: usize) -> Result<Writer> {
    let writer = Writer::new(index, heap)?;
    writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
    Ok(writer)
}

/// The next full byte-size tier or mostly-deleted segment. Both planning and execution use
/// this selection, so an unchanged refresh can skip its writer only when no compaction is pending.
fn compaction_candidates(dir: &Path, index: &Index) -> Result<Option<Vec<tantivy::index::SegmentId>>> {
    compaction_candidates_at_floor(dir, index, TIER_FLOOR)
}

fn compaction_candidates_at_floor(
    dir: &Path,
    index: &Index,
    floor: u64,
) -> Result<Option<Vec<tantivy::index::SegmentId>>> {
    let bytes = |m: &tantivy::index::SegmentMeta| -> u64 {
        m.list_files()
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).ok())
            .map(|md| md.len())
            .sum()
    };
    let tier = |b: u64| -> u32 {
        let mut t = 0;
        let mut cap = floor;
        while b > cap {
            t += 1;
            cap = cap.saturating_mul(TIER_FANOUT as u64);
        }
        t
    };
    let metas = index.searchable_segment_metas().context("listing segments")?;
    let mut tiers: std::collections::BTreeMap<u32, Vec<tantivy::index::SegmentId>> = Default::default();
    let mut pick: Option<Vec<tantivy::index::SegmentId>> = None;
    for m in &metas {
        if m.max_doc() > 0 && m.num_deleted_docs() as f32 / m.max_doc() as f32 > DEAD_RATIO {
            pick = Some(vec![m.id()]);
            break;
        }
        tiers.entry(tier(bytes(m))).or_default().push(m.id());
    }
    Ok(pick.or_else(|| tiers.into_values().find(|ids| ids.len() >= TIER_FANOUT)))
}

/// Merge the committed segments by size tier, synchronously: any tier holding [`TIER_FANOUT`]
/// segments becomes one segment, and a segment past [`DEAD_RATIO`] is rewritten alone, until no
/// rule fires. An unchanged source set can still finish compaction left pending by an earlier run.
/// Bottom-tier outputs can remain in that tier and merge again; deletion cleanup uses document
/// share, not physical deleted-byte share.
/// Byte tiers constrain grouping, but do not guarantee one rewrite per byte per tier.
/// Returns the merges run.
fn compact(dir: &Path, index: &Index, writer: &mut Writer) -> Result<usize> {
    compact_with_candidates(dir, index, writer, compaction_candidates)
}

fn compact_with_candidates(
    dir: &Path,
    index: &Index,
    writer: &mut Writer,
    mut candidates: impl FnMut(&Path, &Index) -> Result<Option<Vec<tantivy::index::SegmentId>>>,
) -> Result<usize> {
    let mut merges = 0;
    loop {
        let pick = candidates(dir, index)?;
        let Some(ids) = pick else { break };
        writer.merge(&ids).wait().context("merging segments")?;
        merges += 1;
        // A merge tantivy declined leaves its inputs in place; stop rather than pick them forever.
        let after: HashSet<_> = index
            .searchable_segment_ids()
            .context("listing segments")?
            .into_iter()
            .collect();
        if ids.iter().all(|id| after.contains(id)) {
            break;
        }
    }
    if merges > 0 {
        writer
            .garbage_collect_files()
            .wait()
            .context("collecting merged-away segment files")?;
    }
    Ok(merges)
}

/// Searchable bytes per tantivy document. A session's body is flushed into **multiple** docs (all
/// sharing the session id) at the first message end past this size, or mid-message at twice it,
/// so the index never holds a whole large session's body at once — search dedups hits back to one
/// row per id. 4 MB keeps per-doc memory bounded while staying well above almost every individual
/// message. 2 KB under test, so the small fixtures span several docs.
const CHUNK_BYTES: usize = if cfg!(all(test, not(feature = "probe-heap"))) {
    2048
} else {
    4 * 1024 * 1024
};

/// Discover + parse every session and bring the index at `dir` up to date.
///
/// Incremental by default: a session whose freshness signature already matches the index is
/// **skipped** (no re-parse cost beyond the metadata scan, no rewrite); changed sessions are
/// replaced; new ones added; and sessions whose source files no longer exist are deleted. Pass
/// `rebuild = true` to clear and rebuild from scratch. Returns the number of sessions discovered
/// (not just the changed ones).
///
/// **Chunked ingestion:** each session streams message-by-message into a bounded body buffer that
/// flushes a tantivy document every [`CHUNK_BYTES`], so a large session is indexed as several small
/// docs (same id) and the indexer never holds its whole body. Commits periodically so segments flush
/// rather than accumulating in the writer's heap.
///
/// **Event ride-along:** the same single adapter pass also feeds the event catalog
/// ([`cv_core::events`] — file edits/reads, commands, errors → `cv events` / `cv touched`) via a
/// [`cv_core::TeeSink`]. Events keep their own `(mtime, size)` skip-table (`event_sync`), so an FTS-only
/// `--rebuild` doesn't force an event re-ingest and a tantivy-fresh session whose events are
/// missing gets an events-only catch-up pass; when both are stale the session is read exactly once.
/// Failed parses or event publications keep successful independent work, clear unsafe resume
/// marks, and report an incomplete index after the sweep finishes.
///
/// **Sub-agent forest (`subagents`):** after each top-level claude session, also walk its
/// `cv_core::subagent_tree_of` forest and index/ingest every Task/Workflow agent transcript, tagged
/// with provenance (parent id / agent id / workflow run). Off by default — the forest can be
/// hundreds of transcripts (~900 MB) — and folded-in sub-agent docs carry the provenance fields so
/// a hit knows which agent of which workflow of which parent it came from.
pub fn index_all(dir: &Path, rebuild: bool, subagents: bool) -> Result<usize> {
    let _sources = cv_core::source::read_scope();
    index_sessions(dir, cv_core::discover_all(), rebuild, subagents)
}

pub fn index_sessions(
    dir: &Path,
    refs: impl AsRef<[cv_core::SessionRef]>,
    rebuild: bool,
    subagents: bool,
) -> Result<usize> {
    let _sources = cv_core::source::read_scope();
    let swept = sweep_refs(dir, refs.as_ref(), rebuild, subagents)?;
    if !rebuild {
        eprintln!(
            "  ↳ index: {} changed/new ({} read from where the last pass stopped), {} removed, {} unchanged, {} errors",
            swept.changed, swept.resumed, swept.removed, swept.unchanged, swept.errors
        );
        if swept.copies != 0 {
            eprintln!(
                "  ↳ {} discovered copies resolved to their preferred logical session",
                swept.copies
            );
        }
    }
    anyhow::ensure!(
        swept.errors == 0,
        "index incomplete: {} session(s) failed: {}; retry cv index",
        swept.errors,
        swept.first_error.as_deref().unwrap_or("unknown error")
    );
    Ok(swept.total)
}

/// Successful-sweep delivered callbacks and document submissions, not input scans or timings.
#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
struct SweepWork {
    callbacks: usize,
    documents: usize,
    body_bytes: usize,
    writer_opened: bool,
}

/// What one [`sweep`] did.
#[derive(Default)]
pub(crate) struct Swept {
    /// Top-level sessions seen.
    pub total: usize,
    /// Discovered duplicate copies omitted from the global-ID index.
    pub copies: usize,
    /// Sessions (top-level or folded sub-agents) whose FTS docs were (re)written.
    pub changed: usize,
    /// Requested logical sessions whose FTS documents were already fresh.
    pub unchanged: usize,
    /// Sessions whose parse or event publication failed (successful independent work is kept).
    pub errors: usize,
    first_error: Option<String>,
    /// Sessions reaped because their files are gone.
    pub removed: usize,
    /// Sessions read only from their mark on.
    pub resumed: usize,
    #[cfg(test)]
    admission: (usize, usize, usize, usize),
    #[cfg(test)]
    work: SweepWork,
    #[cfg(all(test, feature = "probe-heap"))]
    bodies: HashMap<String, BodySummary>,
}

/// Inputs copied into retained body documents. Updated time and file signature live only on
/// the replaceable meta document; body, preview and first-message boundaries come from the
/// verified transcript prefix. Keep None distinct from an empty title or path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiscoveryMetadata {
    id: String,
    harness: String,
    path: String,
    cwd: Option<String>,
    title: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    parent_id: Option<String>,
    agent_id: Option<String>,
    workflow: Option<String>,
}

impl DiscoveryMetadata {
    fn new(r: &cv_core::SessionRef, prov: &cv_core::events::Provenance) -> Self {
        Self {
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            path: r.path.display().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            title: r.title.clone(),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            parent_id: prov.parent_id.clone(),
            agent_id: prov.agent_id.clone(),
            workflow: prov.workflow.clone(),
        }
    }

    fn same_body(&self, other: &Self) -> bool {
        // This time is stamped only on the replaceable meta doc, so an ordinary append
        // may update it while retaining every prefix body document.
        (
            &self.id,
            &self.harness,
            &self.path,
            &self.cwd,
            &self.title,
            self.created_at,
            &self.parent_id,
            &self.agent_id,
            &self.workflow,
        ) == (
            &other.id,
            &other.harness,
            &other.path,
            &other.cwd,
            &other.title,
            other.created_at,
            &other.parent_id,
            &other.agent_id,
            &other.workflow,
        )
    }
}

/// One session a sweep must read, and what for.
struct Job {
    r: cv_core::SessionRef,
    mtime: i64,
    size: i64,
    /// Rewrite its FTS docs (else the pass only feeds the catalog: events and counts).
    fts: bool,
    /// Its events are stale: tee the pass into an [`cv_core::events::EventSink`].
    events: bool,
    /// The index holds docs for its id, to delete before the new ones land.
    existing: bool,
    /// Only growth relative to the indexed source can be an append. Equal-size timestamp changes
    /// and truncations require a whole pass, even if a sampled mark still matches.
    grew: bool,
    source: cv_core::source::Snapshot,
    dependencies_fresh: bool,
    prov: cv_core::events::Provenance,
    /// Where the last pass over this file left off, if it left a mark.
    mark: Option<cv_core::resume::MarkHead>,
}

/// What a sweep worker hands the writer thread, in order per session.
enum Out {
    /// A session's FTS rewrite starts, its old docs (those `delete` names) going first.
    Begin {
        id: String,
        delete: Delete,
    },
    Doc(AdmittedDocument),
    /// A session's read finished: its events (from the message index given on), counts and new
    /// mark are the writer's to record.
    Done {
        job: Box<Job>,
        events: Option<(usize, cv_core::events::EventSpool)>,
        counted: Option<cv_core::stream::Counted>,
        mark: Option<(cv_core::resume::MarkHead, cv_core::stream::Checkpoint)>,
        /// The pass resumed at the job's mark.
        resumed: bool,
        fts_reads: cv_core::source::ReadSet,
        #[cfg(test)]
        callbacks: usize,
    },
    /// A session's read failed: any docs it sent are deleted and it stays stale.
    Failed {
        job: Job,
        err: anyhow::Error,
    },
}

/// Which of a session's docs a rewrite replaces.
#[derive(Clone, Copy)]
enum Delete {
    /// None: the index holds none.
    Nothing,
    All,
    /// A pass resumed at this message: its meta doc and the body docs from here on.
    From(usize),
}

fn source_consumer(dir: &Path) -> Result<String> {
    Ok(format!(
        "fts:{}",
        std::fs::canonicalize(dir)
            .context("identifying search index dependencies")?
            .display()
    ))
}

fn preferred_index_candidates<'a>(
    refs: Vec<(&'a cv_core::SessionRef, &'a cv_core::events::Provenance)>,
) -> Vec<(&'a cv_core::SessionRef, &'a cv_core::events::Provenance)> {
    let mut slots: HashMap<String, usize> = HashMap::new();
    let mut selected: Vec<(&cv_core::SessionRef, &cv_core::events::Provenance)> = Vec::new();
    for (r, prov) in refs {
        if let Some(&slot) = slots.get(&r.id) {
            let (old, old_prov) = selected[slot];
            // Keep source errors meaningful if no live copy exists.
            if (!r.path.exists())
                .cmp(&(!old.path.exists()))
                .then_with(|| r.preferred_copy_cmp(old))
                .then_with(|| prov.parent_id.cmp(&old_prov.parent_id))
                .is_lt()
            {
                selected[slot] = (r, prov);
            }
        } else {
            slots.insert(r.id.clone(), selected.len());
            selected.push((r, prov));
        }
    }
    selected
}

#[cfg(test)]
fn preferred_index_copies(refs: Vec<cv_core::SessionRef>) -> Vec<cv_core::SessionRef> {
    let prov = cv_core::events::Provenance::default();
    preferred_index_candidates(refs.iter().map(|r| (r, &prov)).collect())
        .into_iter()
        .map(|(r, _)| r.clone())
        .collect()
}

/// Bring the index at `dir` up to date with `refs` (the top-level sessions on disk), on every core.
///
/// The main thread plans (the freshness checks), then `available_parallelism` workers each take
/// whole sessions — largest first, so the giant ones never straggle at the end — and parse them
/// once for FTS docs, events and affordance counts. Everything that writes stays on this thread,
/// fed by a bounded channel: the tantivy writer (so a session's delete precedes its docs, in
/// opstamp order) and every catalog write (sqlite has one writer anyway). Peak memory is bounded
/// at both queues: raw document byte reservations survive the producer channel and Tantivy's
/// internal 10,000-document pipeline until indexing consumes each document. The shared
/// [`crate::ingest::DOCUMENT_BYTES`] guard is independent of Tantivy's 50 MB arena budget.
/// Each worker also owns its body buffer and at most one document waiting for admission; parser
/// working sets and merge allocations remain separate. Source pressure additionally stays under
/// [`IN_FLIGHT_BYTES`] of estimated transcript (a larger session runs alone). Interim commits land only when no session is half-sent, so a crash never leaves a
/// truncated session stamped fresh; past [`COMMIT_EVERY`] × 8 uncommitted docs the workers drain
/// to make one.
///
/// **Resumption:** a claude or plain-`.jsonl` codex transcript that has grown since the last pass
/// is read only from that pass's last [`cv_core::stream::Checkpoint`] (its mark, one per file in
/// the catalog, taken every [`CHECKPOINT_EVERY`] bytes) when the mark's sampled guards match
/// ([`cv_core::resume::MarkHead::holds`]) and covers everything this pass feeds: the docs from the
/// mark's message on are replaced, the events from there on, and the counts continue from the
/// mark's. This relies on the adapter's immutable-prefix append contract, not a proof of the
/// whole prefix's contents. Otherwise the session is read whole. New marks are stored only after the commit that
/// holds their docs, so a crash between the two can never leave a mark ahead of the index.
#[cfg(test)]
pub(crate) fn sweep(dir: &Path, refs: Vec<cv_core::SessionRef>, rebuild: bool, subagents: bool) -> Result<Swept> {
    sweep_refs(dir, &refs, rebuild, subagents)
}

fn sweep_refs(dir: &Path, refs: &[cv_core::SessionRef], rebuild: bool, subagents: bool) -> Result<Swept> {
    let (index, f) = open_or_create(dir)?;
    let total = refs.len();
    let copies = total - refs.iter().map(|r| &r.id).collect::<HashSet<_>>().len();
    let planner = SweepPlanner {
        index: &index,
        f: &f,
        dir,
        refs,
        rebuild,
        subagents,
    };
    // Readers can decide an unchanged sweep without starting Tantivy worker threads. If a
    // different writer publishes while we plan, retry against one stable committed snapshot.
    let (plan, planned_meta) = loop {
        let before = std::fs::read(dir.join("meta.json"))?;
        let plan = planner.plan()?;
        let compact = compaction_candidates(dir, &index)?.is_some();
        if std::fs::read(dir.join("meta.json"))? != before {
            continue;
        }
        if !rebuild && plan.jobs.is_empty() && plan.missing.is_empty() && !compact {
            return Ok(Swept {
                total,
                copies,
                unchanged: plan.unchanged,
                ..Default::default()
            });
        }
        break (plan, before);
    };
    let mut writer = open_writer(&index, 50_000_000)?;
    // The mutation plan must describe the metadata protected by the writer lock, rather than
    // a previously observed index. Ordinary single-writer refreshes need no second plan.
    let plan = planner.under_writer(plan, &planned_meta, &writer)?;
    if rebuild {
        writer.delete_all_documents().context("clearing index")?;
        writer.commit().context("commit after clear")?;
    }
    let SweepPlan {
        unchanged,
        jobs,
        missing,
        consumer,
    } = plan;
    let ran = run_jobs(&mut writer, &f, jobs, &consumer)?;

    // Reap sessions that disappeared from disk since the last index (incremental only). A plain
    // refresh never reaps the folded sub-agent forest — see [`missing_ids`].
    let removed = if rebuild {
        0
    } else {
        for id in &missing {
            writer.delete_term(Term::from_field_text(f.id, id));
        }
        missing.len()
    };
    writer.commit().context("committing index")?;
    cv_core::resume::update(&ran.marks);
    cv_core::source::update(&consumer, &ran.dependencies);
    compact(dir, &index, &mut writer)?;
    Ok(Swept {
        total,
        copies,
        changed: ran.changed,
        unchanged,
        removed,
        resumed: ran.resumed,
        errors: ran.errors,
        first_error: ran.first_error,
        #[cfg(test)]
        admission: writer.admission.stats(),
        #[cfg(test)]
        work: SweepWork {
            writer_opened: true,
            ..ran.work
        },
        #[cfg(all(test, feature = "probe-heap"))]
        bodies: ran.bodies,
    })
}

struct SweepPlan {
    unchanged: usize,
    jobs: Vec<Job>,
    missing: Vec<String>,
    consumer: String,
}

struct SweepPlanner<'a> {
    index: &'a Index,
    f: &'a Fields,
    dir: &'a Path,
    refs: &'a [cv_core::SessionRef],
    rebuild: bool,
    subagents: bool,
}

impl SweepPlanner<'_> {
    fn under_writer(&self, plan: SweepPlan, planned_meta: &[u8], _writer: &Writer) -> Result<SweepPlan> {
        if std::fs::read(self.dir.join("meta.json"))? != planned_meta {
            self.plan()
        } else {
            Ok(plan)
        }
    }

    fn plan(&self) -> Result<SweepPlan> {
        let Self {
            index,
            f,
            dir,
            refs,
            rebuild,
            subagents,
        } = *self;
        // id → (mtime, size) for every live doc — the incremental skip-set — plus the set of ids that
        // are folded-in sub-agent docs (they carry `parent_id`), which the reap below must leave alone
        // on a plain (no --subagents) refresh.
        let (existing, folded, metadata, duplicate_meta) = if rebuild {
            Default::default()
        } else {
            read_indexed_metadata(index, f, true)
                .context("reading prior index metadata (use cv index --rebuild to replace corrupt metadata)")?
        };
        // One query each for the event skip-table and the affordance counts `has:<kind>` reads.
        let event_sync = cv_core::events::SyncTable::load();
        let affordances = cv_core::catalog::AffordanceTable::load();
        let marks = cv_core::resume::MarkTable::load();
        let consumer = source_consumer(dir)?;
        let dependencies = cv_core::source::Table::load(&consumer);
        let mut probe = cv_core::source::Probe::default();

        let mut seen: HashSet<String> = HashSet::new();
        let mut jobs: Vec<Job> = Vec::new();
        let mut unchanged = 0;
        // Seal owned folded entries before taking pointers into them. Top-level metadata
        // and provenance stay borrowed until a real Job needs ownership.
        let mut folded_candidates = Vec::new();
        if subagents {
            for r in refs {
                for sub in cv_core::subagent_tree_of(r) {
                    let prov = cv_core::events::Provenance {
                        parent_id: Some(r.id.clone()),
                        agent_id: Some(sub.agent_id().to_string()),
                        workflow: sub.workflow.clone(),
                    };
                    folded_candidates.push((sub.session, prov));
                }
            }
        }
        let top_prov = cv_core::events::Provenance::default();
        let candidates = refs
            .iter()
            .map(|r| (r, &top_prov))
            .chain(folded_candidates.iter().map(|(r, prov)| (r, prov)))
            .collect();
        for (r, prov) in preferred_index_candidates(candidates) {
            seen.insert(r.id.clone());
            let source = probe.snapshot(r);
            let (mtime, size) = source.primary;
            let dependencies_fresh = dependencies.auxiliary_fresh_with_probe(
                r,
                existing.get(&r.id).copied().unwrap_or(source.primary),
                &source.dependencies,
                &mut probe,
            );
            let fts_fresh = fts_is_fresh(r, existing.get(&r.id), mtime, size, || {
                dependencies_fresh && dependencies.primary_fresh(r, &source.dependencies)
            }) && metadata.get(&r.id) == Some(&DiscoveryMetadata::new(r, prov));
            unchanged += usize::from(fts_fresh);
            let events = event_sync.needs_ingest_with_provenance(r, &source, prov, &mut probe);
            // Unchanged on every axis → skip entirely (no re-parse beyond the metadata scan).
            if fts_fresh
                && !events
                && (prov.parent_id.is_some() || affordances.counts_with_probe(r, &source, &mut probe).is_some())
            {
                continue;
            }
            jobs.push(Job {
                r: r.clone(),
                mtime,
                size,
                fts: !fts_fresh,
                events,
                existing: existing.contains_key(&r.id),
                grew: !duplicate_meta.contains(&r.id)
                    && existing
                        .get(&r.id)
                        .is_some_and(|&(_, old_size)| old_size > 0 && size > old_size)
                    && (!events || event_sync.can_resume(r, prov)),
                prov: prov.clone(),
                mark: marks.get(r).cloned(),
                source,
                dependencies_fresh,
            });
        }
        jobs.retain(|j| cv_core::harness::for_harness(j.r.harness).is_some());

        let missing = missing_ids(&existing, &seen, &folded, subagents);
        Ok(SweepPlan {
            unchanged,
            jobs,
            missing,
            consumer,
        })
    }
}

/// Conservative primary-source pressure guard. This is not an allocated-heap estimate: mapped
/// Source JSON may own only descriptors, and a SQLite primary contains other sessions too.
/// Retain it until every adapter's parser working set has an independently enforced bound.
const IN_FLIGHT_BYTES: u64 = 2 << 30;

/// The worker queue admits logical-session groups, largest first. Planning chooses one
/// preferred copy per ID; group reservations bound concurrent source bytes.
struct Queue {
    groups: Vec<Vec<Job>>,
    in_flight: u64,
    draining: bool,
    stopped: bool,
}

/// Return the largest waiting group that fits. The workload is finite, so a large waiting
/// group cannot starve: after fitting work completes it is admitted alone when the guard empties.
impl Queue {
    fn next_fitting(&self) -> Option<usize> {
        if self.stopped || self.draining {
            return None;
        }
        if self.in_flight == 0 {
            return self.groups.len().checked_sub(1);
        }
        self.groups
            .iter()
            .rposition(|group| job_cost(group) <= IN_FLIGHT_BYTES.saturating_sub(self.in_flight))
    }
}

/// Return the parser reservation on every exit, including a disconnected writer.
struct GroupReservation<'a> {
    queue: &'a std::sync::Mutex<Queue>,
    wake: &'a std::sync::Condvar,
    bytes: u64,
}
impl Drop for GroupReservation<'_> {
    fn drop(&mut self) {
        self.queue.lock().unwrap().in_flight -= self.bytes;
        self.wake.notify_all();
    }
}

/// An early writer/commit failure must wake workers parked on either admission condition.
/// This guard drops before thread::scope waits for workers, avoiding disconnected-channel hangs.
struct Shutdown<'a> {
    queue: &'a std::sync::Mutex<Queue>,
    wake: &'a std::sync::Condvar,
    admission: &'a Admission,
    complete: bool,
}
impl Shutdown<'_> {
    fn stop(&self) {
        self.queue.lock().unwrap().stopped = true;
        self.admission.close();
        self.wake.notify_all();
    }
}
impl Drop for Shutdown<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.stop();
        }
    }
}

fn job_cost(group: &[Job]) -> u64 {
    group
        .iter()
        .map(|j| j.size.max(0) as u64)
        .max()
        .unwrap_or(0)
        .min(IN_FLIGHT_BYTES)
}

#[cfg(all(test, feature = "probe-heap"))]
#[derive(serde::Serialize)]
struct BodySummary {
    bytes: usize,
    docs: usize,
    hash: u64,
}
#[cfg(all(test, feature = "probe-heap"))]
impl Default for BodySummary {
    fn default() -> Self {
        Self {
            bytes: 0,
            docs: 0,
            hash: 0xcbf29ce484222325,
        }
    }
}
#[cfg(all(test, feature = "probe-heap"))]
fn body_hash(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
    }
    hash
}

/// What [`run_jobs`] did.
#[derive(Default)]
struct Ran {
    #[cfg(test)]
    work: SweepWork,
    errors: usize,
    first_error: Option<String>,
    /// Sessions whose FTS docs were rewritten.
    changed: usize,
    /// Sessions read from their mark on.
    resumed: usize,
    /// Mark updates still waiting for the commit that holds their docs.
    marks: Vec<cv_core::resume::MarkUpdate>,
    dependencies: Vec<cv_core::source::Update>,
    #[cfg(all(test, feature = "probe-heap"))]
    bodies: HashMap<String, BodySummary>,
}

/// Run every job on a pool of workers, writing on this thread.
fn run_jobs(writer: &mut Writer, f: &Fields, jobs: Vec<Job>, consumer: &str) -> Result<Ran> {
    use std::sync::{mpsc, Condvar, Mutex};
    if jobs.is_empty() {
        return Ok(Ran::default());
    }
    let changing: Vec<_> = jobs.iter().filter(|j| j.fts).map(|j| j.r.clone()).collect();
    anyhow::ensure!(
        cv_core::source::invalidate(consumer, &changing),
        "invalidating old search dependency baselines"
    );
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Vec<Job>> = Vec::new();
    for j in jobs {
        match by_id.get(&j.r.id) {
            Some(&g) => groups[g].push(j),
            None => {
                by_id.insert(j.r.id.clone(), groups.len());
                groups.push(vec![j]);
            }
        }
    }
    // Smallest last, since workers pop from the end: the largest sessions start first.
    groups.sort_by_key(|g| job_cost(g));
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(groups.len());
    let queue = Mutex::new(Queue {
        groups,
        in_flight: 0,
        draining: false,
        stopped: false,
    });
    let wake = Condvar::new();
    let (tx, rx) = mpsc::sync_channel::<Out>(2 * workers);
    let admission = std::sync::Arc::clone(&writer.admission);
    let sources = cv_core::source::read_scope();

    std::thread::scope(|scope| -> Result<Ran> {
        for _ in 0..workers {
            let tx = tx.clone();
            let (queue, wake, admission, sources) = (&queue, &wake, &admission, &sources);
            scope.spawn(move || {
                sources.run(|| loop {
                    let group = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if q.stopped || q.groups.is_empty() {
                                break None;
                            }
                            if let Some(at) = q.next_fitting() {
                                let group = q.groups.remove(at);
                                q.in_flight += job_cost(&group);
                                break Some(group);
                            }
                            q = wake.wait(q).unwrap();
                        }
                    };
                    let Some(group) = group else { return };
                    let _reservation = GroupReservation {
                        queue,
                        wake,
                        bytes: job_cost(&group),
                    };
                    for job in group {
                        if !run_job(job, f, &tx, admission) {
                            return; // the writer is gone
                        }
                    }
                })
            });
        }
        drop(tx);

        let mut shutdown = Shutdown {
            queue: &queue,
            wake: &wake,
            admission: &admission,
            complete: false,
        };
        let mut catalog = cv_core::catalog::Writer::open();
        let mut open: HashSet<String> = HashSet::new();
        let mut since_commit = 0usize;
        let mut ran = Ran::default();
        let mut failure: Option<anyhow::Error> = None;
        for out in rx {
            match out {
                Out::Begin { id, delete } => {
                    match delete {
                        Delete::Nothing => {}
                        Delete::All => {
                            writer.delete_term(Term::from_field_text(f.id, &id));
                        }
                        Delete::From(n) => {
                            if let Err(e) = writer.delete_query(Box::new(resumed_docs(f, &id, n))) {
                                failure.get_or_insert(e.into());
                                shutdown.stop();
                            }
                        }
                    }
                    open.insert(id);
                }
                Out::Doc(doc) => {
                    #[cfg(test)]
                    {
                        ran.work.documents += 1;
                        let inner = doc.inner();
                        if let Some(body) = inner.get_first(f.body).and_then(|value| value.as_str()) {
                            ran.work.body_bytes += body.len();
                            #[cfg(feature = "probe-heap")]
                            if std::env::var_os("CV_SWEEP_HASH_BODY").is_some() {
                                if let Some(id) = inner.get_first(f.id).and_then(|value| value.as_str()) {
                                    let summary = ran.bodies.entry(id.to_owned()).or_default();
                                    summary.bytes += body.len();
                                    summary.docs += 1;
                                    summary.hash = body_hash(summary.hash, body.as_bytes());
                                }
                            }
                        }
                    }
                    if failure.is_none() {
                        if let Err(e) = writer.add_admitted(doc) {
                            failure = Some(e);
                            shutdown.stop();
                        }
                    }
                    since_commit += 1;
                }
                Out::Done {
                    job,
                    events,
                    counted,
                    mark,
                    resumed,
                    fts_reads,
                    #[cfg(test)]
                    callbacks,
                } => {
                    #[cfg(test)]
                    {
                        ran.work.callbacks += callbacks;
                    }
                    // Only stamp events after a complete pass: a parse error leaves the sync rows
                    // untouched so the session is retried next run.
                    let source = counted
                        .as_ref()
                        .map_or_else(|| job.source.clone(), |c| job.source.with_reads(&c.dependencies));
                    let events_published = match events {
                        Some((from, events)) => {
                            let result = match catalog.as_mut() {
                                Some(catalog) => catalog.record_events(&job.r, from, events, &source, &job.prov),
                                None => cv_core::events::record_spooled_with_source(
                                    &job.r, from, events, &source, &job.prov,
                                ),
                            };
                            match result {
                                Ok(()) => true,
                                Err(err) => {
                                    ran.errors += 1;
                                    ran.first_error.get_or_insert_with(|| {
                                        cv_core::ir::truncate(
                                            &format!(
                                                "event publication failed for {} ({}): {err:#}",
                                                job.r.id, job.r.harness
                                            ),
                                            512,
                                        )
                                    });
                                    eprintln!(
                                        "cv-search: event publication failed for {} ({}): {err:#}",
                                        job.r.id, job.r.harness
                                    );
                                    false
                                }
                            }
                        }
                        None => true,
                    };
                    if let Some(c) = counted {
                        match catalog.as_mut() {
                            Some(catalog) => catalog.record_counted(&job.r, &job.source, c),
                            None => c.record_with_source(&job.r, &job.source),
                        }
                    }
                    if mark.is_some() || job.mark.is_some() {
                        // A mark must describe persisted event rows as well as FTS docs. If the
                        // SQL transaction failed, retry the entire event pass rather than resume
                        // past a missing suffix and incorrectly stamp it fresh.
                        ran.marks
                            .push((job.r.clone(), if events_published { mark } else { None }));
                    }
                    ran.resumed += usize::from(resumed);
                    if job.fts {
                        ran.dependencies
                            .push((job.r.clone(), job.source.with_reads(&fts_reads)));
                        ran.changed += 1;
                        open.remove(&job.r.id);
                    }
                }
                Out::Failed { job, err } => {
                    ran.errors += 1;
                    ran.first_error.get_or_insert_with(|| {
                        cv_core::ir::truncate(
                            &format!("parse failed for {} ({}): {err:#}", job.r.id, job.r.harness),
                            512,
                        )
                    });
                    // The session drops out of the index (and its skip-set entry) until a later
                    // run parses it whole — never frozen in as a truncated body stamped fresh.
                    if job.fts {
                        writer.delete_term(Term::from_field_text(f.id, &job.r.id));
                        open.remove(&job.r.id);
                    }
                    if job.mark.is_some() {
                        ran.marks.push((job.r.clone(), None));
                    }
                    eprintln!("cv-search: parse failed for {} ({}): {err:#}", job.r.id, job.r.harness);
                }
            }
            if since_commit >= COMMIT_EVERY && failure.is_none() {
                if open.is_empty() {
                    writer.commit().context("interim commit")?;
                    cv_core::resume::update(&std::mem::take(&mut ran.marks));
                    cv_core::source::update(consumer, &std::mem::take(&mut ran.dependencies));
                    since_commit = 0;
                    let mut q = queue.lock().unwrap();
                    q.draining = false;
                    drop(q);
                    wake.notify_all();
                } else if since_commit >= 8 * COMMIT_EVERY {
                    queue.lock().unwrap().draining = true;
                }
            }
        }
        match failure {
            Some(e) => Err(e),
            None => {
                shutdown.complete = true;
                Ok(ran)
            }
        }
    })
}

/// The docs a pass resumed at message `n` of session `id` replaces: its meta doc and its body
/// docs from message `n` on.
fn resumed_docs(f: &Fields, id: &str, n: usize) -> tantivy::query::BooleanQuery {
    use std::ops::Bound;
    use tantivy::query::{BooleanQuery, Occur, Query, RangeQuery, TermQuery};
    use tantivy::schema::IndexRecordOption;
    let id_q: Box<dyn Query> = Box::new(TermQuery::new(
        Term::from_field_text(f.id, id),
        IndexRecordOption::Basic,
    ));
    let later: Box<dyn Query> = Box::new(RangeQuery::new(
        Bound::Included(Term::from_field_i64(f.first_msg, n as i64)),
        Bound::Unbounded,
    ));
    let which: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
        (Occur::Should, Box::new(kind_query(f, KIND_META)) as Box<dyn Query>),
        (Occur::Should, later),
    ]));
    BooleanQuery::new(vec![(Occur::Must, id_q), (Occur::Must, which)])
}

/// Read one job and send what it found; `false` when the writer hung up.
fn run_job(job: Job, f: &Fields, tx: &std::sync::mpsc::SyncSender<Out>, admission: &std::sync::Arc<Admission>) -> bool {
    let Some(adapter) = cv_core::harness::for_harness(job.r.harness) else {
        return true;
    };
    let version = cv_core::resume::version(adapter.as_ref(), RESUME_TAG);
    let from = version.as_deref().and_then(|v| resumable_from(&job, v));
    // A resume the adapter declines touched nothing: read the session whole instead.
    let resumed = match &from {
        Some(cp) => job_pass(&job, f, tx, admission, adapter.as_ref(), Some(cp)).transpose(),
        None => None,
    };
    let result = resumed.unwrap_or_else(|| {
        job_pass(&job, f, tx, admission, adapter.as_ref(), None).and_then(|p| p.context("a whole pass never declines"))
    });
    let out = match result {
        Ok(p) => {
            #[cfg(test)]
            if let Some(hook) = AFTER_PASS.lock().unwrap().as_mut() {
                hook(&adapter.resume_source(&job.r));
            }
            let mark = version.as_deref().and_then(|v| {
                let cp = p.last?;
                Some((cv_core::resume::MarkHead::of_file(p.source.as_ref()?, &cp, v)?, cp))
            });
            Out::Done {
                job: Box::new(job),
                events: p.events,
                counted: p.counted,
                mark,
                resumed: p.resumed,
                fts_reads: p.fts_reads,
                #[cfg(test)]
                callbacks: p.callbacks,
            }
        }
        Err(err) => Out::Failed { job, err },
    };
    tx.send(out).is_ok()
}

/// Minimum checkpoint spacing: 4 MiB in production (2 KiB in tests). Checkpoints also require
/// a chunk flush or the nominal maximum gap below. The shared amortized scheduler raises this
/// minimum for states larger than a chunk, preserving exact replay history while reducing
/// repeated snapshots; those large states can require a larger append reread suffix.
const CHECKPOINT_EVERY: u64 = CHUNK_BYTES as u64;

/// Nominal maximum checkpoint gap: 32 MiB in production (16 KiB in tests). Adapter boundaries
/// and the shared snapshot-size spacing can delay this request further.
const CHECKPOINT_AT_MOST: u64 = 8 * CHUNK_BYTES as u64;

/// The version of what a mark's `fts` layer and the docs behind it mean, part of every mark's
/// version: bump it when [`FtsState`], the doc layout or [`CHECKPOINT_EVERY`] changes.
const RESUME_TAG: &str = "fts2";

/// The checkpoint `job` can resume from: the source grew, its sampled mark still matches, and
/// its state covers every consumer this pass feeds. Growth is interpreted under the adapter's
/// immutable-prefix append contract; the mark's samples do not prove the whole prefix unchanged.
fn resumable_from(job: &Job, version: &str) -> Option<cv_core::stream::Checkpoint> {
    if !job.grew || !job.dependencies_fresh {
        return None;
    }
    let head = job.mark.as_ref()?;
    if !head.holds(&cv_core::source::primary_path(&job.r), job.size.max(0) as u64, version) {
        return None;
    }
    let cp = cv_core::resume::state(&job.r, head)?;
    cp.messages()?;
    if job.fts {
        let state: FtsState = cp.get(FTS_LAYER)?;
        if !job.existing
            || !state
                .discovery_metadata
                .same_body(&DiscoveryMetadata::new(&job.r, &job.prov))
        {
            return None;
        }
    }
    if job.events && cv_core::events::EventSink::resume(&cp).is_none() {
        return None;
    }
    Some(cp)
}

/// What one pass over a job's session found.
struct JobPass {
    #[cfg(test)]
    callbacks: usize,
    resumed: bool,
    fts_reads: cv_core::source::ReadSet,
    counted: Option<cv_core::stream::Counted>,
    events: Option<(usize, cv_core::events::EventSpool)>,
    last: Option<cv_core::stream::Checkpoint>,
    /// The resume source, opened before the pass: its mark describes this file, not whatever the
    /// path names once the pass is over.
    source: Option<std::fs::File>,
}

/// Tests: runs between a pass and the mark taken for it, with the resume source's path.
#[cfg(test)]
type AfterPass = Box<dyn FnMut(&std::path::Path) + Send>;
#[cfg(test)]
static AFTER_PASS: std::sync::Mutex<Option<AfterPass>> = std::sync::Mutex::new(None);

/// Count actual adapter callbacks, not the cumulative counts restored from a checkpoint.
#[cfg(test)]
struct CallbackCounter<'a> {
    inner: &'a mut dyn cv_core::MessageSink,
    callbacks: usize,
}
#[cfg(test)]
impl cv_core::MessageSink for CallbackCounter<'_> {
    fn message(&mut self, message: cv_core::Message) -> cv_core::Flow {
        self.callbacks += 1;
        self.inner.message(message)
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.inner.take_error()
    }
    fn meta(&mut self, session: &cv_core::Session) {
        self.inner.meta(session);
    }
    fn dependency(&mut self, path: &Path) {
        self.inner.dependency(path);
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        self.inner.wants_checkpoint(offset)
    }
    fn checkpoint(&mut self, checkpoint: &mut cv_core::stream::Checkpoint) {
        self.inner.checkpoint(checkpoint);
    }
}

/// One pass over `job`'s session — from `from` when given — feeding what the job needs: FTS docs
/// to the writer, events, counts. `Ok(None)`: the adapter declined to resume, nothing was sent.
fn job_pass(
    job: &Job,
    f: &Fields,
    tx: &std::sync::mpsc::SyncSender<Out>,
    admission: &std::sync::Arc<Admission>,
    adapter: &dyn cv_core::Adapter,
    from: Option<&cv_core::stream::Checkpoint>,
) -> Result<Option<JobPass>> {
    let first = from.and_then(|cp| cp.messages()).unwrap_or(0);
    let source = std::fs::File::open(adapter.resume_source(&job.r)).ok();
    let mut events = match from {
        None => cv_core::events::SpoolingSink::new(job.r.cwd.clone()),
        Some(cp) => cv_core::events::SpoolingSink::resume(cp).context("checkpoint without events")?,
    };
    if !job.events {
        events = events.without_events();
    }
    #[cfg(test)]
    let mut measured = CallbackCounter {
        inner: &mut events,
        callbacks: 0,
    };
    #[cfg(test)]
    let event_sink: &mut dyn cv_core::MessageSink = &mut measured;
    #[cfg(not(test))]
    let event_sink: &mut dyn cv_core::MessageSink = &mut events;
    let done = if job.fts {
        let delete = match (from, job.existing) {
            (Some(_), _) => Delete::From(first),
            (None, true) => Delete::All,
            (None, false) => Delete::Nothing,
        };
        // The rewrite begins with the first doc (every pass that runs ends in the meta doc), so a
        // declined resume sends nothing at all.
        let mut begun = false;
        let mut send = |doc: TantivyDocument| -> Result<()> {
            if !begun {
                tx.send(Out::Begin {
                    id: job.r.id.clone(),
                    delete,
                })
                .map_err(|_| anyhow::anyhow!("index writer closed"))?;
                begun = true;
            }
            tx.send(Out::Doc(admission.document(doc)?))
                .map_err(|_| anyhow::anyhow!("index writer closed"))
        };
        index_session(
            &mut send,
            f,
            adapter,
            &job.r,
            job.mtime,
            job.size,
            Some(event_sink),
            job.prov.clone(),
            from,
        )?
        .map(|(_, done, reads)| (done, reads))
    } else {
        let opts = cv_core::ParseOptions::lazy();
        cv_core::stream::stream_resumable_amortized(adapter, &job.r, &opts, event_sink, CHECKPOINT_EVERY, from)?
            .map(|done| (done, cv_core::source::ReadSet::default()))
    };
    let Some((done, reads)) = done else { return Ok(None) };
    #[cfg(test)]
    let callbacks = measured.callbacks;
    let events = if job.events {
        Some((first, events.into_spool()?))
    } else {
        None
    };
    Ok(Some(JobPass {
        #[cfg(test)]
        callbacks,
        fts_reads: reads,
        resumed: from.is_some(),
        counted: done.counted,
        events,
        last: done.last,
        source,
    }))
}

/// Where a session's chunk docs go: the sweep's writer thread, or (tests) straight into a writer.
type DocOut<'a> = dyn FnMut(TantivyDocument) -> Result<()> + 'a;

/// Stream one session into bounded tantivy docs through [`ChunkSink`] — teeing the same pass into
/// `events` when given, so the transcript is read once for both consumers — from `from` when
/// given (`events` restored from it too). `lazy()` keeps large content as spans: the chunk sink
/// chunk-resolves them and the event sink never reads them at all. Returns the docs written and
/// what the pass found; `Ok(None)` when the adapter declined to resume (no doc was written).
#[allow(clippy::too_many_arguments)]
fn index_session(
    out: &mut DocOut<'_>,
    f: &Fields,
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    mtime: i64,
    size: i64,
    events: Option<&mut dyn cv_core::MessageSink>,
    prov: cv_core::events::Provenance,
    from: Option<&cv_core::stream::Checkpoint>,
) -> Result<Option<(usize, cv_core::stream::Resumed, cv_core::source::ReadSet)>> {
    let opts = cv_core::ParseOptions::lazy();
    let mut sink = match from {
        Some(cp) => ChunkSink::resume(out, f, r, mtime, size, prov, cp).context("checkpoint without fts state")?,
        None => ChunkSink::new(out, f, r, mtime, size, prov),
    };
    let done = match events {
        Some(es) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, es);
            cv_core::stream::stream_resumable_amortized(adapter, r, &opts, &mut tee, CHECKPOINT_EVERY, from)?
        }
        None => cv_core::stream::stream_resumable_amortized(adapter, r, &opts, &mut sink, CHECKPOINT_EVERY, from)?,
    };
    let Some(done) = done else { return Ok(None) };
    sink.finish()?;
    Ok(Some((sink.docs, done, sink.dependencies)))
}

/// [`index_session`] straight into `writer`, guaranteeing the writer holds **no** docs for the
/// session on error, and recording what the pass counted (tests drive this; the sweep's writer
/// thread does the same with [`Out::Failed`]).
///
/// A large session flushes chunk docs into the writer *while* streaming, so a mid-stream parse
/// error would otherwise leave a truncated head behind — and since every chunk doc is stamped with
/// the full current `(mtime, size)`, [`fts_is_fresh`] would then skip the session forever, freezing
/// the truncation in. Deleting by the shared id term on the error path removes those partial docs
/// (tantivy applies operations in opstamp order, so this cannot touch docs added by a later pass);
/// the id drops out of the skip-set entirely and is retried on the next run.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn index_session_clean(
    writer: &mut Writer,
    f: &Fields,
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    mtime: i64,
    size: i64,
    events: Option<&mut dyn cv_core::MessageSink>,
    prov: cv_core::events::Provenance,
) -> Result<(usize, cv_core::source::ReadSet)> {
    let source = cv_core::source::Probe::default().snapshot(r);
    let result = {
        let mut add = |doc: TantivyDocument| -> Result<()> { writer.add_document(doc).map(|_| ()) };
        index_session(&mut add, f, adapter, r, mtime, size, events, prov, None)
            .and_then(|done| done.context("a whole pass never declines"))
    };
    match result {
        Ok((docs, done, reads)) => {
            if let Some(c) = done.counted {
                c.record_with_source(r, &source);
            }
            Ok((docs, reads))
        }
        Err(e) => {
            writer.delete_term(Term::from_field_text(f.id, &r.id));
            Err(e)
        }
    }
}

/// Select vanished sessions, preserving unvisited folded subagents on a plain refresh.
fn missing_ids(
    existing: &HashMap<String, (i64, i64)>,
    seen: &HashSet<String>,
    folded: &HashSet<String>,
    subagents: bool,
) -> Vec<String> {
    existing
        .keys()
        .filter(|id| !seen.contains(*id) && (subagents || !folded.contains(*id)))
        .cloned()
        .collect()
}

/// The two kinds of doc a session is indexed as (the `kind` field).
const KIND_META: &str = "meta";
const KIND_BODY: &str = "body";

/// Streams one session's messages into bounded-size body docs (all sharing the session id), so a
/// large session never materializes its whole body in the index, then writes the session's meta
/// doc.
struct ChunkSink<'w> {
    out: &'w mut DocOut<'w>,
    f: &'w Fields,
    id: String,
    harness: String,
    cwd: Option<String>,
    path: String,
    primary_path: PathBuf,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    mtime: i64,
    size: i64,
    disc_title: Option<String>,
    meta_title: Option<String>,
    meta_received: bool,
    /// The head of the first user text (the title fallback), at most [`FIRST_USER_CHARS`].
    first_user: Option<String>,
    /// Sub-agent attribution stamped on every doc of a folded-in forest transcript; empty
    /// (all-`None`) for a top-level session, leaving its docs unchanged.
    prov: cv_core::events::Provenance,
    resolver: cv_core::Resolver,
    dependencies: cv_core::source::ReadSet,
    discovery_metadata: DiscoveryMetadata,
    buf: String,
    /// Messages seen so far: the index of the message being appended.
    msgs: usize,
    /// The index of the message that put the first byte into `buf`.
    buf_first: usize,
    /// A chunk was flushed since the last checkpoint: take the next one offered.
    boundary_due: bool,
    /// Where the last checkpoint was taken (the file offset).
    last_checkpoint: u64,
    /// Whitespace-collapsed head of the body, capped at [`PREVIEW_CHARS`]; stored on the meta doc.
    preview: String,
    docs: usize,
    err: Option<anyhow::Error>,
    failed: bool,
}

/// The [`cv_core::stream::Checkpoint`] layer a [`ChunkSink`] keeps its state under.
const FTS_LAYER: &str = "fts";

/// What a [`ChunkSink`] holds at a checkpoint (its buffer is empty there: the docs so far are in
/// the index).
#[derive(serde::Serialize, serde::Deserialize)]
struct FtsState {
    discovery_metadata: DiscoveryMetadata,
    #[serde(default = "cv_core::source::ReadSet::unrecorded")]
    dependencies: cv_core::source::ReadSet,
    msgs: usize,
    cwd: Option<String>,
    meta_title: Option<String>,
    meta_received: bool,
    first_user: Option<String>,
    preview: String,
}

/// Length cap (chars) of the stored head-of-body preview — the snippet fallback when a hit's
/// source file is gone at query time. ~240 bytes/session keeps the index lean.
const PREVIEW_CHARS: usize = 240;

/// How much of the first user text the sink keeps: [`cv_core::label_from`] reads 72 chars of it.
const FIRST_USER_CHARS: usize = 512;

impl<'w> ChunkSink<'w> {
    fn new(
        out: &'w mut DocOut<'w>,
        f: &'w Fields,
        r: &cv_core::SessionRef,
        mtime: i64,
        size: i64,
        prov: cv_core::events::Provenance,
    ) -> Self {
        ChunkSink {
            out,
            f,
            discovery_metadata: DiscoveryMetadata::new(r, &prov),
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            path: r.path.display().to_string(),
            primary_path: cv_core::source::primary_path(r),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            mtime,
            size,
            disc_title: r.title.clone(),
            meta_title: None,
            meta_received: false,
            first_user: None,
            prov,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
            dependencies: Default::default(),
            buf: String::new(),
            msgs: 0,
            buf_first: 0,
            boundary_due: false,
            last_checkpoint: 0,
            preview: String::new(),
            docs: 0,
            err: None,
            failed: false,
        }
    }

    /// The sink a checkpoint kept, for a pass resumed there (the signature, dates and title from
    /// discovery are this pass's, as a whole pass would have them). `None` when the checkpoint was
    /// taken without one.
    fn resume(
        out: &'w mut DocOut<'w>,
        f: &'w Fields,
        r: &cv_core::SessionRef,
        mtime: i64,
        size: i64,
        prov: cv_core::events::Provenance,
        cp: &cv_core::stream::Checkpoint,
    ) -> Option<Self> {
        let st: FtsState = cp.get(FTS_LAYER)?;
        if !st.discovery_metadata.same_body(&DiscoveryMetadata::new(r, &prov)) {
            return None;
        }
        let mut sink = ChunkSink::new(out, f, r, mtime, size, prov);
        sink.dependencies = st.dependencies;
        sink.msgs = st.msgs;
        sink.cwd = st.cwd;
        sink.meta_title = st.meta_title;
        sink.meta_received = st.meta_received;
        sink.first_user = st.first_user;
        sink.preview = st.preview;
        sink.last_checkpoint = cp.offset;
        Some(sink)
    }

    /// Session label for display: the parsed title when the adapter delivered metadata (authoritative,
    /// e.g. codex's None → first-user fallback), else the discovery-time title (claude's ai-title).
    fn title(&self) -> String {
        let primary = if self.meta_received {
            self.meta_title.as_deref()
        } else {
            self.disc_title.as_deref()
        };
        cv_core::label_from(primary, self.first_user.as_deref())
    }

    /// The fields every doc of the session carries.
    fn doc(&self, kind: &str) -> TantivyDocument {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.f.id, &self.id);
        doc.add_text(self.f.kind, kind);
        doc.add_text(self.f.harness, &self.harness);
        if let Some(cwd) = &self.cwd {
            doc.add_text(self.f.cwd, cwd);
        }
        doc.add_text(self.f.title, self.title());
        doc.add_text(self.f.path, &self.path);
        if let Some(t) = self.created_at {
            doc.add_i64(self.f.created_at, t);
        }
        if kind == KIND_META {
            // This type contains only infallibly serializable scalars.
            doc.add_text(
                self.f.discovery_metadata,
                serde_json::to_string(&self.discovery_metadata).unwrap(),
            );
        }
        // `updated_at` moves with every append; only the meta doc, rewritten by every pass, has it.
        if let (Some(t), KIND_META) = (self.updated_at, kind) {
            doc.add_i64(self.f.updated_at, t);
        }
        // Provenance rides every doc of a folded-in sub-agent; top-level docs add nothing here.
        if let Some(p) = &self.prov.parent_id {
            doc.add_text(self.f.parent_id, p);
        }
        if let Some(a) = &self.prov.agent_id {
            doc.add_text(self.f.agent_id, a);
        }
        if let Some(w) = &self.prov.workflow {
            doc.add_text(self.f.workflow, w);
        }
        doc
    }

    fn emit(&mut self, doc: TantivyDocument) {
        if let Err(e) = (self.out)(doc) {
            self.err = Some(e);
        }
        self.docs += 1;
    }

    /// Flush the current buffer as one body doc (nothing when it is empty).
    fn flush(&mut self) {
        if self.failed || self.err.is_some() || self.buf.is_empty() {
            return;
        }
        let mut doc = self.doc(KIND_BODY);
        doc.add_text(self.f.body, &self.buf);
        doc.add_i64(self.f.first_msg, self.buf_first as i64);
        self.emit(doc);
        self.buf.clear();
    }

    /// Flush the trailing buffer, then write the meta doc — the session's freshness signature,
    /// current label and preview, one per session — propagating any deferred error.
    fn finish(&mut self) -> Result<()> {
        self.flush();
        if !self.failed && self.err.is_none() {
            let mut doc = self.doc(KIND_META);
            if !self.preview.is_empty() {
                doc.add_text(self.f.preview, &self.preview);
            }
            doc.add_i64(self.f.mtime, self.mtime);
            // `size` is the primary incremental freshness key for append-only transcript logs.
            doc.add_i64(self.f.size, self.size);
            self.emit(doc);
        }
        if let Some(e) = self.err.take() {
            self.failed = true;
            return Err(e);
        }
        anyhow::ensure!(!self.failed, "search sink has already failed");
        Ok(())
    }
}

impl<'w> ChunkSink<'w> {
    /// Append at UTF-8 boundaries, flushing before an inline field can exceed two chunks.
    fn push_chunk(&mut self, s: &str) {
        self.extend_preview(s);
        self.push_body(s);
    }

    fn push_body(&mut self, mut s: &str) {
        let cap = 2 * CHUNK_BYTES;
        while !s.is_empty() && !self.failed && self.err.is_none() {
            if self.buf.is_empty() {
                self.buf_first = self.msgs;
            }
            let take = floor_char(s, (cap - self.buf.len()).min(s.len()));
            if take == 0 {
                self.flush_chunk();
                continue;
            }
            // String's geometric growth must not create an allocation beyond the document cap.
            if self.buf.capacity().saturating_mul(2) > cap {
                self.buf.reserve_exact(take);
            }
            self.buf.push_str(&s[..take]);
            s = &s[take..];
            if self.buf.len() == cap || !s.is_empty() {
                self.flush_chunk();
            }
        }
    }

    /// A chunk's worth is in: flush it, and take the next checkpoint offered (which flushes what
    /// came after it in the same record), so checkpoints add no doc boundaries of their own beyond
    /// that remainder.
    fn flush_chunk(&mut self) {
        self.flush();
        self.boundary_due = true;
    }

    /// Grow the stored preview from the head of the body: words joined by single spaces (newlines
    /// and runs of whitespace collapse away), truncated at [`PREVIEW_CHARS`]. No-op once full, so
    /// the cost is bounded no matter how large the session is.
    fn extend_preview(&mut self, s: &str) {
        let mut n = self.preview.chars().count();
        if n >= PREVIEW_CHARS {
            return;
        }
        for word in s.split_whitespace() {
            if n >= PREVIEW_CHARS {
                break;
            }
            if !self.preview.is_empty() {
                self.preview.push(' ');
                n += 1;
            }
            for c in word.chars() {
                if n >= PREVIEW_CHARS {
                    break;
                }
                self.preview.push(c);
                n += 1;
            }
        }
    }

    fn record_span(&mut self, sp: &cv_core::Span) {
        if let Some(path) = sp.source.as_ref().filter(|path| path.as_path() != self.primary_path) {
            self.dependencies.record(path);
        }
    }
    fn record_text(&mut self, text: &cv_core::Text) {
        match text {
            cv_core::Text::Span(span) => self.record_span(span),
            cv_core::Text::Source(source) => {
                if source.path() != self.primary_path {
                    self.dependencies.record(source.path());
                }
                if let Some(reads) = source.dependencies() {
                    self.dependencies.merge(reads);
                }
            }
            cv_core::Text::Inline(_) => {}
        }
    }
    /// Append a content `Text`: chunk-resolved from the source for a span (never owned whole), or the
    /// inline string. `r` is the session resolver (moved out of `self` to avoid a borrow conflict with
    /// the chunk callback, which mutates `self`).
    fn append_text(&mut self, r: &cv_core::Resolver, text: &cv_core::Text) {
        if let Some(text) = text.inline_str() {
            self.push_chunk(text);
            return;
        }
        self.record_text(text);
        if let Err(error) = text.try_for_each_chunk(r, CHUNK_BYTES, |s| {
            self.push_chunk(s);
            anyhow::ensure!(!self.failed && self.err.is_none(), "search document output failed");
            Ok(())
        }) {
            if !self.failed && self.err.is_none() {
                self.err = Some(error.context("resolving search text"));
            }
        }
    }

    /// Append one block's searchable text — identical projection to
    /// [`cv_core::ir::Message::append_searchable`], but chunk-resolving span content.
    fn append_block(&mut self, r: &cv_core::Resolver, b: &cv_core::Block) {
        use cv_core::Block;
        if self.failed || self.err.is_some() {
            return;
        }
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => {
                self.append_text(r, text);
                self.push_chunk("\n");
            }
            Block::ToolUse { name, input, .. } => {
                if let cv_core::json::Json::Source(range) = input {
                    if range.path() != self.primary_path {
                        self.dependencies.record(range.path());
                    }
                    if let Some(reads) = range.dependencies() {
                        self.dependencies.merge(reads);
                    }
                }
                self.push_chunk(name);
                self.push_chunk(" ");
                // Canonical JSON streams into bounded body documents without owning a value tree.
                if let Err(error) = input.write_canonical(&mut JsonChunks::new(self)) {
                    if !self.failed && self.err.is_none() {
                        self.err = Some(error.context("serializing tool input for search"));
                    }
                }
                self.push_chunk("\n");
            }
            Block::ToolResult { content, .. } => {
                self.append_text(r, content);
                if self.failed || self.err.is_some() {
                    return;
                }
                self.push_chunk("\n");
                // A persisted-output stub (Claude Code parked the real output in
                // `<session>/tool-results/<id>.txt`): index the file's head too, so the text the tool
                // produced is findable even though the transcript holds only the pointer.
                if let Some(p) = b.persisted_output_path() {
                    self.dependencies.record(Path::new(p));
                    match cv_core::lazy::read_head_checked(std::path::Path::new(p), PERSISTED_INDEX_CAP) {
                        Ok(Some(t)) => {
                            for piece in t.as_bytes().chunks(CHUNK_BYTES) {
                                self.push_chunk(&String::from_utf8_lossy(piece));
                            }
                            self.push_chunk("\n");
                        }
                        Ok(None) => {}
                        Err(error) => {
                            if !self.failed && self.err.is_none() {
                                self.err = Some(error.context("reading persisted search output"));
                            }
                        }
                    }
                }
            }
            Block::File { path, source, .. } => {
                if let Some(p) = path.as_deref().or(source.as_deref()) {
                    self.push_chunk(p);
                    self.push_chunk("\n");
                }
            }
            Block::Image { .. } => {}
        }
    }
}

/// Canonical writer fragments are UTF-8; the reducer owns splitting and output failures.
struct JsonChunks<'a, 'w> {
    sink: &'a mut ChunkSink<'w>,
    preview_chars: usize,
    word_open: bool,
}
impl<'a, 'w> JsonChunks<'a, 'w> {
    fn new(sink: &'a mut ChunkSink<'w>) -> Self {
        Self {
            preview_chars: sink.preview.chars().count(),
            sink,
            word_open: false,
        }
    }
}
impl std::io::Write for JsonChunks<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let s =
            std::str::from_utf8(bytes).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        // Formatter fragments can split a word. Carry its state across writes so the preview
        // stays identical to extend_preview on the former single serialized string.
        if self.preview_chars < PREVIEW_CHARS {
            for c in s.chars() {
                if self.preview_chars >= PREVIEW_CHARS {
                    break;
                }
                if c.is_whitespace() {
                    self.word_open = false;
                } else {
                    if !self.word_open && !self.sink.preview.is_empty() {
                        self.sink.preview.push(' ');
                        self.preview_chars += 1;
                    }
                    if self.preview_chars < PREVIEW_CHARS {
                        self.sink.preview.push(c);
                        self.preview_chars += 1;
                    }
                    self.word_open = true;
                }
            }
        }
        self.sink.push_body(s);
        if self.sink.failed || self.sink.err.is_some() {
            Err(std::io::Error::other("search document output failed"))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How much of a persisted tool output (a sidecar file) to index per stub. A multi-MB dump beyond
/// this is still on disk (`cv show` prints the path); the index just carries its head.
const PERSISTED_INDEX_CAP: usize = 1024 * 1024;

impl cv_core::MessageSink for ChunkSink<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.failed |= self.err.is_some();
        self.err
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("search sink has already failed")))
    }
    fn dependency(&mut self, path: &Path) {
        self.dependencies.record(path);
    }
    fn meta(&mut self, s: &cv_core::Session) {
        self.meta_title = s.title.clone();
        self.meta_received = true;
        if self.cwd.is_none() {
            self.cwd = s.cwd.as_ref().map(|p| p.display().to_string());
        }
    }

    /// Once a chunk was flushed since the last one, so checkpoints sit at (or just after) the
    /// doc boundaries the chunking makes anyway (the more boundaries, the fewer conjunctive
    /// queries match within one doc); or after [`CHECKPOINT_AT_MOST`] of transcript without one,
    /// which only a stretch with little text reaches (giant tool outputs, telemetry), so a resumed
    /// pass normally re-reads little. The shared scheduler can delay requests for large states.
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        !self.failed
            && self.err.is_none()
            && (self.boundary_due || offset >= self.last_checkpoint.saturating_add(CHECKPOINT_AT_MOST))
    }

    /// A checkpoint is a body-doc boundary: every doc lies wholly before it or after it, so a
    /// pass resumed there replaces exactly the docs from its message on.
    fn checkpoint(&mut self, cp: &mut cv_core::stream::Checkpoint) {
        self.flush();
        self.boundary_due = false;
        self.last_checkpoint = cp.offset;
        cp.put(
            FTS_LAYER,
            &FtsState {
                discovery_metadata: self.discovery_metadata.clone(),
                dependencies: self.dependencies.clone(),
                msgs: self.msgs,
                cwd: self.cwd.clone(),
                meta_title: self.meta_title.clone(),
                meta_received: self.meta_received,
                first_user: self.first_user.clone(),
                preview: self.preview.clone(),
            },
        );
    }
    fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
        use cv_core::Block;
        if self.failed || self.err.is_some() {
            return cv_core::Flow::Stop;
        }
        // Move the resolver out so the chunk callbacks (which mutate `self`) don't conflict with it.
        let r = std::mem::replace(&mut self.resolver, cv_core::Resolver::new(None));
        // First user text → the title fallback. Only its first 72 chars are ever read
        // (`label_from` truncates), so a span resolves just a 512-byte head, never the whole field.
        if self.first_user.is_none() && m.role == cv_core::Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    self.record_text(text);
                    let s = match text.try_resolve_prefix(&r, 512) {
                        Ok(text) => text,
                        Err(error) => {
                            self.err = Some(error.context("reading search label"));
                            break;
                        }
                    };
                    let t = s.trim();
                    if !t.is_empty() {
                        self.first_user = Some(t.chars().take(FIRST_USER_CHARS).collect());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            self.append_block(&r, b);
        }
        self.resolver = r;
        self.msgs += 1;
        if self.buf.len() >= CHUNK_BYTES {
            self.flush_chunk();
        }
        if self.failed || self.err.is_some() {
            cv_core::Flow::Stop
        } else {
            cv_core::Flow::Continue
        }
    }
}

/// Whether an indexed session is FTS-fresh given its stored `(mtime, size)` and the current file
/// signature. Both must match, including for append logs: a same-size edit is still a change.
/// A real empty file requires fresh captured source authority. Unreadable `(0, 0)` tuples
/// and missing/legacy guards still miss. Pre-`size` indexes rebuild in [`open_or_create`].
fn fts_is_fresh(
    _r: &cv_core::SessionRef,
    stored: Option<&(i64, i64)>,
    mtime: i64,
    size: i64,
    source_fresh: impl FnOnce() -> bool,
) -> bool {
    stored.is_some_and(|&stored| cv_core::offsets::sig_is_fresh_with_source(stored, (mtime, size), source_fresh))
}

/// `id → (mtime, size)` for every live doc (the incremental skip-set), plus the set of ids that
/// are folded-in sub-agent docs (those tagged with a `parent_id`), which a plain refresh's reap
/// must leave alone.
type IndexedSigs = (HashMap<String, (i64, i64)>, HashSet<String>);

/// Read the [`IndexedSigs`] off every live meta doc in the index (one per session; stored fields
/// are tiny, so this is cheap).
fn read_indexed_sigs(index: &Index, f: &Fields) -> Result<IndexedSigs> {
    let (sigs, folded, _, _) = read_indexed_metadata(index, f, false)?;
    Ok((sigs, folded))
}

type IndexedMetadata = (
    HashMap<String, (i64, i64)>,
    HashSet<String>,
    HashMap<String, DiscoveryMetadata>,
    HashSet<String>,
);

fn read_indexed_metadata(index: &Index, f: &Fields, read_metadata: bool) -> Result<IndexedMetadata> {
    use tantivy::collector::DocSetCollector;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();
    let mut out = HashMap::new();
    let mut folded = HashSet::new();
    let mut metadata = HashMap::new();
    let mut duplicates = HashSet::new();
    let metas = searcher
        .search(&kind_query(f, KIND_META), &DocSetCollector)
        .context("listing meta docs")?;
    for addr in metas {
        let doc: TantivyDocument = searcher.doc(addr).context("reading indexed metadata document")?;
        let id = doc
            .get_first(f.id)
            .and_then(|v| v.as_str())
            .context("indexed metadata is missing a string session id; use cv index --rebuild")?
            .to_owned();
        let mt = doc.get_first(f.mtime).and_then(|v| v.as_i64());
        let sz = doc.get_first(f.size).and_then(|v| v.as_i64());
        // Keep identity even when the incremental stamp is malformed: the session must be
        // deleted/replaced, rather than admitted as new alongside its existing stale meta row.
        let (mt, sz) = mt.zip(sz).unwrap_or((i64::MIN, i64::MIN));
        {
            if doc.get_first(f.parent_id).is_some() {
                folded.insert(id.clone());
            }
            if read_metadata {
                if let Some(stamp) = doc
                    .get_first(f.discovery_metadata)
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                {
                    metadata.insert(id.clone(), stamp);
                }
            }
            if out.insert(id.clone(), (mt, sz)).is_some() && read_metadata {
                duplicates.insert(id);
            }
        }
    }
    for id in &duplicates {
        metadata.remove(id);
    }
    Ok((out, folded, metadata, duplicates))
}

/// Matches the docs of one `kind`.
fn kind_query(f: &Fields, kind: &str) -> tantivy::query::TermQuery {
    tantivy::query::TermQuery::new(
        Term::from_field_text(f.kind, kind),
        tantivy::schema::IndexRecordOption::Basic,
    )
}

/// Session `id`'s meta doc. Preserve best-effort metadata fallback on lookup/read failure.
fn meta_doc(searcher: &tantivy::Searcher, f: &Fields, id: &str) -> Option<TantivyDocument> {
    searcher.doc(meta_addr(searcher, f, id).ok()??).ok()
}

fn meta_addr(searcher: &tantivy::Searcher, f: &Fields, id: &str) -> Result<Option<tantivy::DocAddress>> {
    use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
    use tantivy::schema::IndexRecordOption;
    let id_q: Box<dyn Query> = Box::new(TermQuery::new(
        Term::from_field_text(f.id, id),
        IndexRecordOption::Basic,
    ));
    let q = BooleanQuery::new(vec![
        (Occur::Must, id_q),
        (Occur::Must, Box::new(kind_query(f, KIND_META))),
    ]);
    let top = searcher.search(&q, &TopDocs::with_limit(1).order_by_score())?;
    Ok(top.into_iter().next().map(|(_, addr)| addr))
}

#[derive(Clone, Copy)]
enum MetadataDecision {
    Present {
        address: tantivy::DocAddress,
        accepted: bool,
    },
    Missing,
    Unavailable,
}

/// Query-local predicate decisions and addresses. Retain no metadata strings/documents: only
/// matching IDs demand this map, and only final winners reread their metadata for display.
struct MetadataDecisions {
    searcher: tantivy::Searcher,
    fields: Fields,
    harness: Option<String>,
    cwd: Option<String>,
    cache: std::sync::Mutex<HashMap<String, MetadataDecision>>,
    #[cfg(test)]
    queries: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    stored_reads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    winner_body_reads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    winner_metadata_reads: std::sync::atomic::AtomicUsize,
}
impl MetadataDecisions {
    fn new(searcher: &tantivy::Searcher, fields: &Fields, filter: crate::SearchFilter<'_>) -> Self {
        Self {
            searcher: searcher.clone(),
            fields: fields.clone(),
            harness: filter.harness.map(str::to_owned),
            cwd: filter.cwd.map(str::to_owned),
            cache: Default::default(),
            #[cfg(test)]
            queries: Default::default(),
            #[cfg(test)]
            stored_reads: Default::default(),
            #[cfg(test)]
            winner_body_reads: Default::default(),
            #[cfg(test)]
            winner_metadata_reads: Default::default(),
        }
    }
    fn resolve(&self, id: &str) -> MetadataDecision {
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .copied();
        if let Some(decision) = cached {
            return decision;
        }
        // Errors remain retryable and keep the existing per-body fallback. A definite missing
        // meta doc is stable in this searcher's snapshot, but its body filter is segment-local.
        #[cfg(test)]
        self.queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let decision = match meta_addr(&self.searcher, &self.fields, id) {
            Ok(Some(address)) => {
                #[cfg(test)]
                self.stored_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match self.searcher.doc::<TantivyDocument>(address) {
                    Ok(doc) => MetadataDecision::Present {
                        address,
                        accepted: metadata_matches(&doc, &self.fields, self.harness.as_deref(), self.cwd.as_deref()),
                    },
                    Err(_) => return MetadataDecision::Unavailable,
                }
            }
            Ok(None) => MetadataDecision::Missing,
            Err(_) => return MetadataDecision::Unavailable,
        };
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.to_owned(), decision);
        decision
    }
    fn cached(&self, id: &str) -> Option<MetadataDecision> {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .copied()
    }
}
fn metadata_matches(doc: &TantivyDocument, fields: &Fields, harness: Option<&str>, cwd: Option<&str>) -> bool {
    harness.is_none_or(|wanted| doc.get_first(fields.harness).and_then(|value| value.as_str()) == Some(wanted))
        && cwd.is_none_or(|needle| {
            doc.get_first(fields.cwd)
                .and_then(|value| value.as_str())
                .is_some_and(|value| value.contains(needle))
        })
}

/// The newest file mtime (ns) stamped on any indexed doc — how far the index has caught up. `None`
/// when the index can't be opened or is empty. One stored-field scan, the same cost incremental
/// indexing pays for its skip-set.
pub fn newest_indexed_mtime(dir: &Path) -> Option<i64> {
    let (index, f) = open_existing(dir).ok()?;
    let (sigs, _) = read_indexed_sigs(&index, &f).ok()?;
    sigs.values().filter(|(_, size)| *size >= 0).map(|&(mt, _)| mt).max()
}

/// How far the index at `dir` trails the live session corpus — the data the search footer's
/// staleness disclosure names. `stale_sessions` counts sessions whose primary source signature no
/// longer matches their indexed stamp (grew, rewrote, or arrived since the last pass); `behind_ns`
/// is the gap between the index's newest stamp and the newest session file on disk. `None` when
/// the index can't be opened or holds no meta docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staleness {
    pub stale_sessions: usize,
    pub behind_ns: i64,
}

/// The staleness probe. The sessions come from the catalog ([`cv_core::sessions`], which probes
/// its freshness and rescans only what changed), then one stat each — the same cost this call's
/// miss-path predecessor paid; a full `discover_all` here made every miss pay a fleet scan.
pub fn staleness(dir: &Path) -> Option<Staleness> {
    staleness_over(dir, &cv_core::sessions())
}

/// [`staleness`] over a caller-supplied inventory — the split that lets a unit test feed the
/// fixture's refs rather than a real corpus discovery.
fn staleness_over(dir: &Path, refs: &[cv_core::SessionRef]) -> Option<Staleness> {
    let (index, f) = open_existing(dir).ok()?;
    let (sigs, _) = read_indexed_sigs(&index, &f).ok()?;
    let indexed = sigs.values().filter(|(_, size)| *size >= 0).map(|&(mt, _)| mt).max()?;
    let mut stale_sessions = 0usize;
    let mut newest_on_disk = 0i64;
    for r in refs {
        let cur = cv_core::offsets::file_sig(&cv_core::source::primary_path(r));
        newest_on_disk = newest_on_disk.max(cur.0);
        if !sigs
            .get(&r.id)
            .is_some_and(|&stored| cv_core::offsets::sig_is_fresh(stored, cur))
        {
            stale_sessions += 1;
        }
    }
    Some(Staleness {
        stale_sessions,
        behind_ns: newest_on_disk.saturating_sub(indexed),
    })
}

/// The one-sentence staleness disclosure every search surface prints — on a hit list it warns the
/// rows may be behind, on a miss it names the likely reason a recent conversation wasn't found.
/// `None` when the index is caught up (or unmeasurable).
pub fn staleness_note(dir: &Path) -> Option<String> {
    staleness(dir).as_ref().and_then(staleness_line)
}

/// The note text for a measured [`Staleness`], `None` when caught up — the pure split the unit
/// test asserts on (`staleness` itself goes through the fleet catalog's discovery).
fn staleness_line(s: &Staleness) -> Option<String> {
    if s.stale_sessions == 0 {
        return None;
    }
    Some(format!(
        "the index is ~{} behind for {} session(s) — run `cv index`",
        fmt_age_ns(s.behind_ns),
        s.stale_sessions
    ))
}

/// `7s` / `14m` / `9h` / `3d` — the compact age the staleness note carries.
fn fmt_age_ns(ns: i64) -> String {
    let secs = (ns / 1_000_000_000).max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Whether the index holds any folded-in sub-agent transcripts (the `cv index --subagents` forest,
/// detected by a stored `parent_id` on a doc). The empty-result path uses this to nudge toward
/// `--subagents` **only** when the forest is genuinely absent — not when a query simply didn't
/// match a corpus that already includes it. An open/read error reads as `true` (assume present) so
/// a transient failure never spams the hint. One stored-field scan — the same cheap pass
/// [`newest_indexed_mtime`] makes (no body is stored).
pub fn has_subagent_docs(dir: &Path) -> bool {
    let Ok((index, f)) = open_existing(dir) else {
        return true;
    };
    match read_indexed_sigs(&index, &f) {
        Ok((_, folded)) => !folded.is_empty(),
        Err(_) => true,
    }
}

/// Index an explicit set of sessions through the real production [`index_session`] path — the same
/// indexer [`sweep`] drives, minus only the global `discover_all()` scan and the worker pool. Full
/// rebuild: clears prior contents first. Used by tests so date-field (and chunk) indexing is
/// exercised against the code that actually ships rather than a parallel per-doc path. `catalog =
/// true` additionally tees each pass into the event catalog, like the sweep does (only set it under
/// an isolated `CLUSTERVISION_HOME` — it writes the shared catalog db).
#[cfg(test)]
pub(crate) fn index_refs(dir: &Path, refs: &[cv_core::SessionRef], catalog: bool) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer = open_writer(&index, 50_000_000)?;
    let consumer = source_consumer(dir)?;
    anyhow::ensure!(
        cv_core::source::invalidate(&consumer, refs),
        "invalidating old search dependency baselines"
    );
    writer.delete_all_documents().context("clearing index")?;
    let mut probe = cv_core::source::Probe::default();
    let mut dependencies = Vec::new();
    for r in refs {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let source = probe.snapshot(r);
        let (mtime, size) = source.primary;
        let mut es = catalog.then(|| cv_core::events::EventSink::new(r.cwd.clone()));
        let (_, reads) = index_session_clean(
            &mut writer,
            &f,
            adapter.as_ref(),
            r,
            mtime,
            size,
            es.as_mut().map(|s| s as &mut dyn cv_core::MessageSink),
            cv_core::events::Provenance::default(),
        )
        .with_context(|| format!("indexing {}", r.id))?;
        if let Some(es) = es {
            cv_core::events::record_from_with_source(
                r,
                0,
                es.events(),
                &source.with_reads(es.dependencies()),
                &Default::default(),
            );
        }
        dependencies.push((r.clone(), source.with_reads(&reads)));
    }
    writer.commit().context("committing index")?;
    cv_core::source::update(&consumer, &dependencies);
    Ok(refs.len())
}

/// The production incremental [`sweep`] over an explicit set of sessions (as a plain, no
/// `--subagents` refresh), minus only the global `discover_all()` scan (which can't be pointed at
/// a temp dir). Unlike [`index_refs`] it does **not** clear first, so a second call with unchanged
/// files exercises the skip. Returns the number of sessions actually (re)indexed — the count the
/// regression test asserts goes to 0 on a pure mtime bump and to 1 on a real append.
#[cfg(test)]
pub(crate) fn index_refs_incremental(dir: &Path, refs: &[cv_core::SessionRef]) -> Result<usize> {
    Ok(sweep(dir, refs.to_vec(), false, false)?.changed)
}

/// Index `refs` (top-level, cleared first through [`index_refs`]) **and** their sub-agent forests
/// through the production `--subagents` [`sweep`] — minus only the global `discover_all()` scan.
/// `catalog = true` tees the top-level pass into the event catalog (set it only under an isolated
/// `CLUSTERVISION_HOME`). Returns the number of sub-agent transcripts folded in.
#[cfg(test)]
pub(crate) fn index_refs_with_subagents(dir: &Path, refs: &[cv_core::SessionRef], catalog: bool) -> Result<usize> {
    index_refs(dir, refs, catalog)?;
    Ok(sweep(dir, refs.to_vec(), false, true)?.changed)
}

fn parse_text_query(index: &Index, f: &Fields, query: &str) -> Result<Box<dyn tantivy::query::Query>> {
    let mut parser = QueryParser::for_index(index, vec![f.title, f.body, f.cwd]);
    parser.set_conjunction_by_default();
    parser
        .parse_query(query)
        .with_context(|| format!("parsing query {query:?}"))
}

/// The complete set of session IDs matching a full-text query. This reads only the index:
/// no ranking cap, snippet generation, discovery or transcript access. Chunk documents deduplicate
/// into their session ID, with the same query syntax and conjunction default as ranked search.
pub fn text_search_ids(dir: &Path, query: &str) -> Result<HashSet<String>> {
    let (index, f) = open_existing(dir)?;
    let reader = index.reader().context("opening index reader")?;
    matching_ids(&index, &f, &reader.searcher(), query)
}

/// Resolve several needles against one reader snapshot. Setup failures fail the operation;
/// malformed individual queries retain their own errors without discarding other needles.
pub fn text_search_ids_batch(dir: &Path, queries: &[&str]) -> Result<Vec<Result<HashSet<String>>>> {
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let (index, f) = open_existing(dir)?;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();
    Ok(queries
        .iter()
        .map(|query| matching_ids(&index, &f, &searcher, query))
        .collect())
}

fn matching_ids(index: &Index, f: &Fields, searcher: &tantivy::Searcher, query: &str) -> Result<HashSet<String>> {
    let parsed = parse_text_query(index, f, query)?;
    searcher
        .search(&parsed, &crate::session_collector::SessionIds(f.id))
        .context("matching session IDs")
}

/// Add live snippets to selected ranked hits. Only these source transcripts are read; unavailable
/// sources keep their stored preview, with the same moved-file annotation as [`text_search`].
pub fn enrich_text_snippets(dir: &Path, query: &str, hits: &mut [Hit]) -> Result<()> {
    if hits.is_empty() {
        return Ok(());
    }
    let (index, f) = open_existing(dir)?;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();
    for hit in hits {
        let doc = meta_doc(&searcher, &f, &hit.id);
        let path = doc.as_ref().and_then(|d| d.get_first(f.path)).and_then(|v| v.as_str());
        if let Some(snippet) = path
            .and_then(|p| live_snippet(p, &hit.harness, query))
            .filter(|s| !s.is_empty())
        {
            hit.snippet = snippet;
        } else if path.is_none_or(|p| !Path::new(p).exists()) {
            hit.snippet = if hit.snippet.is_empty() {
                "(source file moved)".to_string()
            } else {
                format!("{} (source file moved)", hit.snippet)
            };
        }
    }
    Ok(())
}

/// Run a full-text query, returning up to `limit` hits ranked by BM25, each with a live snippet and
/// its `created_at`/`updated_at` (so callers needn't re-discover the corpus just to date the rows).
/// If the hit's source file can't be re-read (moved/changed since indexing), the snippet falls back
/// to the head-of-body preview stored at index time, suffixed with `(source file moved)` when the
/// file is gone entirely.
///
/// The query supports tantivy's syntax: bare terms search title+body+cwd, and fielded terms like
/// `harness:claude`, phrases `"foo bar"`, and booleans `a AND b` work too.
pub fn text_search(dir: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let mut hits = text_search_indexed(dir, query, limit)?;
    enrich_text_snippets(dir, query, &mut hits)?;
    Ok(hits)
}

/// Rank indexed sessions using their stored previews, without reading source transcripts.
/// Call [`enrich_text_snippets`] after filtering and truncating the displayed hits.
pub fn text_search_indexed(dir: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
    text_search_indexed_filtered(dir, query, limit, crate::SearchFilter::default())
}

/// Apply session metadata filters before selecting the best ranked sessions.
pub fn text_search_indexed_filtered(
    dir: &Path,
    query: &str,
    limit: usize,
    filter: crate::SearchFilter<'_>,
) -> Result<Vec<Hit>> {
    if !dir.join("meta.json").exists() {
        anyhow::bail!("no full-text index at {} — run `index_all` first", dir.display());
    }
    // Read-only open: a transient failure here must surface as an error, never as a wiped index.
    let (index, f) = open_existing(dir)?;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();

    let decisions = std::sync::Arc::new(MetadataDecisions::new(&searcher, &f, filter));
    ranked_search(&index, &f, &searcher, query, limit, filter, decisions)
}

fn ranked_search(
    index: &Index,
    f: &Fields,
    searcher: &tantivy::Searcher,
    query: &str,
    limit: usize,
    filter: crate::SearchFilter<'_>,
    decisions: std::sync::Arc<MetadataDecisions>,
) -> Result<Vec<Hit>> {
    let parsed = parse_text_query(index, f, query)?;

    // Collect sessions rather than chunks. Each segment's top `limit` sessions contains every
    // possible global winner, even when one session has arbitrarily many matching chunks.
    let mut collector = crate::session_collector::TopSessions::new(f.id, limit);
    if filter.harness.is_some() || filter.cwd.is_some() {
        let harness = filter.harness.map(str::to_owned);
        let cwd = filter.cwd.map(str::to_owned);
        let fields = f.clone();
        let shared = decisions.clone();
        collector.filter = Some(std::sync::Arc::new(move |id, store, doc_id| {
            // Resumed chunks can retain older labels/cwd: filter the same current meta doc that
            // describes the returned hit, falling back only for legacy body-only documents.
            match shared.resolve(id) {
                MetadataDecision::Present { accepted, .. } => {
                    return Ok(crate::session_collector::FilterDecision::Session(accepted))
                }
                MetadataDecision::Missing | MetadataDecision::Unavailable => {}
            }
            let fallback = store.get::<TantivyDocument>(doc_id)?;
            Ok(crate::session_collector::FilterDecision::Document(metadata_matches(
                &fallback,
                &fields,
                harness.as_deref(),
                cwd.as_deref(),
            )))
        }));
    }
    let ranked = searcher.search(&parsed, &collector).context("running search")?;

    let mut hits = Vec::with_capacity(ranked.len());
    for candidate in ranked {
        let (score, id) = (candidate.score, candidate.id);
        let head: crate::session_collector::StoredHead<2> =
            searcher.doc(candidate.addr).context("loading stored doc header")?;
        let load_candidate = || -> Result<TantivyDocument> {
            #[cfg(test)]
            decisions
                .winner_body_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            searcher.doc(candidate.addr).context("loading stored doc")
        };
        // Preserve candidate-meta identity (including compatible duplicate metadata rows).
        // A body winner needs its current meta document; only legacy/missing/unreadable
        // metadata or a reordered header demands the complete candidate document.
        let (is_meta, candidate_doc) = match head.value(f.kind) {
            Some(kind) => (kind.as_str() == Some(KIND_META), None),
            None => {
                let doc = load_candidate()?;
                let is_meta = doc.get_first(f.kind).and_then(|v| v.as_str()) == Some(KIND_META);
                (is_meta, Some(doc))
            }
        };
        let doc = if is_meta {
            match candidate_doc {
                Some(doc) => doc,
                None => load_candidate()?,
            }
        } else {
            let metadata = match decisions.cached(&id) {
                Some(MetadataDecision::Present { address, .. }) => {
                    #[cfg(test)]
                    decisions
                        .winner_metadata_reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    searcher.doc(address).ok()
                }
                Some(MetadataDecision::Missing) => None,
                _ => meta_doc(searcher, f, &id),
            };
            match metadata {
                Some(doc) => doc,
                None => match candidate_doc {
                    Some(doc) => doc,
                    None => load_candidate()?,
                },
            }
        };
        let get_str =
            |field: Field| -> Option<String> { doc.get_first(field).and_then(|v| v.as_str()).map(|s| s.to_string()) };
        let harness = get_str(f.harness).unwrap_or_default();
        let snippet = get_str(f.preview).unwrap_or_default();

        hits.push(Hit {
            id,
            harness,
            cwd: get_str(f.cwd),
            title: get_str(f.title),
            created_at: doc.get_first(f.created_at).and_then(|v| v.as_i64()),
            updated_at: doc.get_first(f.updated_at).and_then(|v| v.as_i64()),
            score,
            snippet,
            parent_id: get_str(f.parent_id),
            agent_id: get_str(f.agent_id),
            workflow: get_str(f.workflow),
        });
    }
    Ok(hits)
}

/// Most text we'll scan from a session to find a snippet window. Matches are almost always early;
/// capping keeps snippeting a multi-GB hit bounded.
const SNIPPET_SCAN_CAP: usize = 256 * 1024;

/// Generate a snippet for a hit by re-reading **just that one session** (streamed, capped), finding
/// the first query term, and windowing around it — instead of keeping a stored copy of every body.
///
/// Streams under [`ParseOptions::lazy`] and resolves span content **capped at the remaining scan
/// budget**, so a session fronted by a giant record (e.g. a 700 MB tool dump) costs at most
/// [`SNIPPET_SCAN_CAP`] here — the old `bulk()` + materialize path owned the whole record before
/// the cap check could run. Trade-off: a term that only occurs *beyond* the cap inside one giant
/// field now falls back to the head/preview snippet (it already did for terms in later messages).
fn live_snippet(path: &str, harness: &str, query: &str) -> Option<String> {
    use cv_core::{Block, Flow, Message, MessageSink, ParseOptions, SessionRef};
    let h = cv_core::Harness::parse(harness)?;
    let adapter = cv_core::harness::for_harness(h)?;
    let sref = SessionRef {
        id: String::new(),
        harness: h,
        path: PathBuf::from(path),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };

    struct Acc {
        head: cv_core::projection::Head,
        resolver: cv_core::Resolver,
        error: Option<anyhow::Error>,
        failed: bool,
    }
    impl MessageSink for Acc {
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.failed |= self.error.is_some();
            self.error.take().or_else(|| {
                self.failed
                    .then(|| anyhow::anyhow!("snippet projection previously failed"))
            })
        }
        fn message(&mut self, m: Message) -> Flow {
            if self.failed || self.error.is_some() {
                return Flow::Stop;
            }
            for b in &m.content {
                if self.head.full() {
                    break;
                }
                match b {
                    Block::Text { text } | Block::Thinking { text, .. } => {
                        if let Err(error) = self.head.try_push_text(text, &self.resolver) {
                            self.error = Some(error);
                            return Flow::Stop;
                        }
                        self.head.push("\n");
                    }
                    Block::ToolUse { name, input, .. } => {
                        self.head.push(name);
                        self.head.push(" ");
                        if let Err(error) = self.head.push_json(input) {
                            self.error = Some(error);
                            return Flow::Stop;
                        }
                        self.head.push("\n");
                    }
                    Block::ToolResult { content, .. } => {
                        if let Err(error) = self.head.try_push_text(content, &self.resolver) {
                            self.error = Some(error);
                            return Flow::Stop;
                        }
                        self.head.push("\n");
                        if let Some(p) = b.persisted_output_path() {
                            let remaining = self.head.remaining();
                            if remaining > 0 {
                                match cv_core::lazy::read_head_checked(std::path::Path::new(p), remaining) {
                                    Ok(Some(t)) => {
                                        self.head.push(&t);
                                        self.head.push("\n");
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        self.error = Some(error);
                                        return Flow::Stop;
                                    }
                                }
                            }
                        }
                    }
                    Block::File { path, source, .. } => {
                        if let Some(p) = path.as_deref().or(source.as_deref()) {
                            self.head.push(p);
                            self.head.push("\n");
                        }
                    }
                    Block::Image { .. } => {}
                }
            }
            if self.head.full() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    let mut acc = Acc {
        head: cv_core::projection::Head::new(SNIPPET_SCAN_CAP),
        resolver: cv_core::Resolver::new(Some(PathBuf::from(path))),
        error: None,
        failed: false,
    };
    cv_core::stream::stream_counted(&*adapter, &sref, &ParseOptions::lazy(), &mut acc).ok()?;
    Some(make_snippet(acc.head.as_str(), query))
}

/// Window the body around the first matching query term; fall back to a head if nothing matches.
fn make_snippet(body: &str, query: &str) -> String {
    let lc = body.to_lowercase();
    for term in query_terms(query) {
        if let Some(pos) = lc.find(&term) {
            let source = cv_core::scan::lowercase_source_range(body, pos..pos + term.len());
            return window(body, source.start, source.len());
        }
    }
    head(body, 200)
}

/// The bare, matchable terms of a tantivy query: drops `field:value` filters and boolean operators,
/// keeps lowercased word tokens. Used only to locate a snippet window, not for retrieval.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| !t.contains(':'))
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT"))
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn window(body: &str, pos: usize, len: usize) -> String {
    let start = floor_char(body, pos.saturating_sub(40));
    let end = ceil_char(body, (pos + len + 80).min(body.len()));
    let s = &body[start..end];
    let s = s.replace('\n', " ");
    if s.chars().count() > 200 {
        let t: String = s.chars().take(199).collect();
        format!("{t}…")
    } else {
        s
    }
}

/// Remove a single session from the index by id (handy for incremental updates).
#[allow(dead_code)]
pub fn delete(dir: &Path, id: &str) -> Result<()> {
    let (index, f) = open_existing(dir)?;
    let mut writer = open_writer(&index, 15_000_000)?;
    writer.delete_term(Term::from_field_text(f.id, id));
    writer.commit()?;
    Ok(())
}

fn head(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingest_job(r: cv_core::SessionRef) -> Job {
        let source = cv_core::source::Probe::default().snapshot(&r);
        let (mtime, size) = source.primary;
        Job {
            r,
            mtime,
            size,
            fts: true,
            events: true,
            existing: false,
            grew: false,
            source,
            dependencies_fresh: true,
            prov: Default::default(),
            mark: None,
        }
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "explicit synthetic CV_SWEEP_INPUT_PROBE and CV_SWEEP_INPUT_BYTES, one isolated process"]
    fn production_sweep_heap_probe() {
        assert_eq!(CHUNK_BYTES, 4 * 1024 * 1024);
        let home = IsolatedHome::new();
        let fixture = std::fs::canonicalize(std::env::var_os("CV_SWEEP_INPUT_PROBE").unwrap()).unwrap();
        let input_bytes: usize = std::env::var("CV_SWEEP_INPUT_BYTES").unwrap().parse().unwrap();
        let copies: usize = std::env::var("CV_SWEEP_COPIES")
            .unwrap_or_else(|_| "4".into())
            .parse()
            .unwrap();
        let small: usize = std::env::var("CV_SWEEP_SMALL")
            .unwrap_or_else(|_| "32".into())
            .parse()
            .unwrap();
        assert!((1..=16).contains(&copies));
        assert!(small <= 1000);
        let mut refs = Vec::new();
        for n in 0..copies {
            refs.push(cv_core::SessionRef::new(
                format!("large{n}"),
                cv_core::Harness::Claude,
                &fixture,
            ));
        }
        for n in 0..small {
            let id = format!("small{n}");
            let path = home.home.join(format!("{id}.jsonl"));
            let line = serde_json::json!({"type":"user", "sessionId":id, "uuid":"u",
                "message":{"role":"user", "content":format!("zqsmall{n} small session")}});
            std::fs::write(&path, format!("{line}\n")).unwrap();
            refs.push(cv_core::SessionRef::new(id, cv_core::Harness::Claude, path));
        }
        let dir = tmpdir();
        let baseline = cv_testkit::heap::begin();
        let started = std::time::Instant::now();
        let result = sweep(&dir, refs.clone(), false, false).unwrap();
        let wall = started.elapsed().as_secs_f64();
        let heap = cv_testkit::heap::end(baseline);
        assert_eq!((result.changed, result.errors), (refs.len(), 0));
        assert_eq!(result.admission.0, 0);
        let bounded = std::env::var_os("CV_SWEEP_COMPARE_UNBOUNDED").is_none();
        if bounded {
            assert!(result.admission.1 <= crate::ingest::DOCUMENT_BYTES.max(result.admission.2));
        }
        // This is a scoped composed-path ceiling, not a universal RSS promise. Four concurrent
        // synthetic giant Source inputs include raw queues, working buffers, tokenizers and arenas.
        if copies == 4 && bounded {
            assert!(heap.peak_above_baseline_bytes < 384 * 1024 * 1024);
        }
        let hits = text_search(&dir, "Inspect", copies + 10).unwrap();
        assert_eq!(hits.len(), copies);
        for (n, r) in refs.iter().enumerate() {
            assert!(!cv_core::events::needs_ingest(
                r,
                cv_core::events::file_mtime_ns(&r.path)
            ));
            assert!(cv_core::catalog::AffordanceTable::load().counts(r).is_some());
            if n < copies {
                let events = cv_core::events::events_for("claude", &r.id, None);
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].target.as_deref(), Some("/perf/a.rs"));
            } else {
                let hits = text_search(&dir, &format!("zqsmall{}", n - copies), 10).unwrap();
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].id, r.id);
            }
        }
        if std::env::var_os("CV_SWEEP_HASH_BODY").is_some() {
            let prefix = b"Inspect the file\nEdit {\"file_path\":\"/perf/a.rs\",\"content\":\"";
            let suffix = b"\",\"metadata\":{\"n\":3,\"flag\":true}}\n";
            let mut expected = body_hash(0xcbf29ce484222325, prefix);
            let x = [b'x'; 4096];
            for _ in 0..input_bytes / x.len() {
                expected = body_hash(expected, &x);
            }
            expected = body_hash(expected, &x[..input_bytes % x.len()]);
            expected = body_hash(expected, suffix);
            for n in 0..copies {
                let body = &result.bodies[&format!("large{n}")];
                assert_eq!(body.bytes, prefix.len() + input_bytes + suffix.len());
                assert_eq!(body.hash, expected);
            }
        }
        let repeat_started = std::time::Instant::now();
        let unchanged = sweep(&dir, refs, false, false).unwrap();
        assert_eq!((unchanged.changed, unchanged.errors, unchanged.admission.1), (0, 0, 0));
        eprintln!(
            "{}",
            serde_json::json!({"input_bytes":input_bytes,"physical_bytes":std::fs::metadata(&fixture).unwrap().len(),
            "large_sessions":copies,"small_sessions":small,"chunk_bytes":CHUNK_BYTES,
            "document_limit_bytes":if bounded { Some(crate::ingest::DOCUMENT_BYTES) } else { None },"queue_peak_charged_bytes":result.admission.1,
            "largest_document_charged_bytes":result.admission.2,"queue_waits":result.admission.3,
            "body_hashing":std::env::var_os("CV_SWEEP_HASH_BODY").is_some(),"bodies":result.bodies,
            "heap":heap,"wall_seconds":wall,"unchanged_wall_seconds":repeat_started.elapsed().as_secs_f64(),
            "tantivy_tokenization":true,"commits_and_compaction":true})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_admission_selects_fitting_work_without_losing_largest_first_order() {
        let mut small = ingest_job(cv_core::SessionRef::new("small", cv_core::Harness::Claude, "/missing"));
        small.size = (IN_FLIGHT_BYTES / 8) as i64;
        let mut medium = ingest_job(cv_core::SessionRef::new("medium", cv_core::Harness::Claude, "/missing"));
        medium.size = (IN_FLIGHT_BYTES / 2) as i64;
        let mut large = ingest_job(cv_core::SessionRef::new("large", cv_core::Harness::Claude, "/missing"));
        large.size = (3 * IN_FLIGHT_BYTES / 4) as i64;
        let mut queue = Queue {
            groups: vec![vec![small], vec![medium], vec![large]],
            in_flight: 0,
            draining: false,
            stopped: false,
        };
        assert_eq!(queue.next_fitting(), Some(2));
        let large = queue.groups.pop().unwrap();
        queue.in_flight = job_cost(&large);
        assert_eq!(
            queue.next_fitting(),
            Some(0),
            "do not leave a worker idle behind the medium group"
        );
        let small = queue.groups.remove(0);
        queue.in_flight += job_cost(&small);
        assert_eq!(queue.next_fitting(), None);
        queue.in_flight = 0;
        assert_eq!(queue.next_fitting(), Some(0), "the waiting medium group now runs");
        queue.draining = true;
        assert_eq!(queue.next_fitting(), None);
        queue.draining = false;
        queue.stopped = true;
        assert_eq!(queue.next_fitting(), None);
    }

    #[test]
    fn mixed_sweep_keeps_large_and_small_sessions_exact_under_document_admission() {
        let home = IsolatedHome::new();
        let mut refs = Vec::new();
        for n in 0..18 {
            let id = format!("mixed{n}");
            let marker = format!("zqmixed{n}");
            let path = home.home.join(format!("{id}.jsonl"));
            let size = if n % 6 == 0 { 50 * CHUNK_BYTES } else { 40 };
            let line = serde_json::json!({"type":"assistant", "uuid":"a", "sessionId":id,
                "message":{"role":"assistant", "content":[
                    {"type":"text", "text":format!("{marker} {}", "x".repeat(size))},
                    {"type":"tool_use", "id":"call", "name":"Edit", "input":{"file_path":format!("/scratch/{id}.rs"), "new_string":"y".repeat(size)}}]}});
            std::fs::write(&path, format!("{line}\n")).unwrap();
            refs.push(cv_core::SessionRef::new(&id, cv_core::Harness::Claude, &path));
        }
        let dir = tmpdir();
        let result = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!((result.changed, result.errors), (refs.len(), 0));
        let (live, peak, largest, _) = result.admission;
        assert_eq!(live, 0, "commit consumed every accepted document");
        assert!(peak <= crate::ingest::DOCUMENT_BYTES.max(largest));
        assert!(peak > 0);
        for (n, r) in refs.iter().enumerate() {
            let hits = text_search(&dir, &format!("zqmixed{n}"), 30).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, r.id);
            assert!(!cv_core::events::needs_ingest(
                r,
                cv_core::events::file_mtime_ns(&r.path)
            ));
            assert!(cv_core::catalog::AffordanceTable::load().counts(r).is_some());
        }
        assert_eq!(sweep(&dir, refs, false, false).unwrap().changed, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writer_failure_cancels_sweep_admission_without_publishing_the_partial_session() {
        let home = IsolatedHome::new();
        let path = home.home.join("large.jsonl");
        let line = serde_json::json!({"type":"assistant", "uuid":"a", "sessionId":"large",
            "message":{"role":"assistant", "content":[{"type":"text", "text":"zqpartial ".repeat(300 * CHUNK_BYTES)}]}});
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let r = cv_core::SessionRef::new("large", cv_core::Harness::Claude, &path);
        let dir = tmpdir();
        let (index, fields) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        writer.document_limit(1); // every document runs alone; later producers park on its byte lease
        writer.fail_after(2);
        let admission = std::sync::Arc::clone(&writer.admission);
        let consumer = source_consumer(&dir).unwrap();
        // The active source fills the conservative source guard, parking both other groups.
        // A fatal writer failure must wake those workers before thread::scope tries to join them.
        let mut jobs = vec![
            ingest_job(cv_core::SessionRef::new("later1", cv_core::Harness::Claude, &path)),
            ingest_job(cv_core::SessionRef::new("later2", cv_core::Harness::Claude, &path)),
            ingest_job(r.clone()),
        ];
        for job in &mut jobs {
            job.size = IN_FLIGHT_BYTES as i64;
        }
        let error = run_jobs(&mut writer, &fields, jobs, &consumer).err().unwrap();
        assert!(format!("{error:#}").contains("injected index consumer failure"));
        drop(writer);
        assert_eq!(
            admission.stats().0,
            0,
            "all accepted/discarded documents return their bytes"
        );
        assert_eq!(index.reader().unwrap().searcher().num_docs(), 0);
        assert!(cv_core::events::SyncTable::load().needs_ingest(
            &r,
            cv_core::events::file_mtime_ns(&r.path),
            std::fs::metadata(&r.path).unwrap().len() as i64
        ));
        assert!(cv_core::resume::MarkTable::load().get(&r).is_none());
        assert!(cv_core::catalog::AffordanceTable::load().counts(&r).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tool_input_json_streaming_preserves_unicode_and_canonical_body() {
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("input", cv_core::Harness::Claude, "/absent.jsonl");
        let input = serde_json::json!({
            "first": "é🙂中\\\"\n".repeat(CHUNK_BYTES),
            "nested": [null, true, 4.25, {"key":"尾巴"}],
            "last": "done"
        });
        let expected = format!("Edit {input}\n");
        let mut ignore = |_doc: TantivyDocument| -> Result<()> { Ok(()) };
        let mut former = ChunkSink::new(&mut ignore, &f, &r, 1, 1, Default::default());
        former.extend_preview("Edit");
        former.extend_preview(&input.to_string());
        let expected_preview = former.preview;
        let mut bodies = Vec::new();
        let mut out = |doc: TantivyDocument| -> Result<()> {
            if let Some(body) = doc.get_first(f.body).and_then(|value| value.as_str()) {
                assert!(body.len() <= 2 * CHUNK_BYTES);
                bodies.push(body.to_owned());
            }
            Ok(())
        };
        let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
        sink.append_block(
            &cv_core::Resolver::new(None),
            &cv_core::Block::ToolUse {
                id: "call".into(),
                name: "Edit".into(),
                input: input.into(),
                namespace: None,
            },
        );
        assert!(sink.buf.capacity() <= 2 * CHUNK_BYTES);
        assert_eq!(sink.preview, expected_preview);
        sink.finish().unwrap();
        drop(sink);
        assert!(bodies.len() > 1);
        assert_eq!(bodies.concat(), expected);
    }

    #[test]
    fn source_carriers_stream_exact_chunks_and_keep_auxiliary_guards() {
        let dir = tmpdir();
        let path = dir.join("source.json");
        let conditional = dir.join("database-wal");
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("source", cv_core::Harness::Claude, &path);
        for source_text in [true, false] {
            let decoded = "source label é🙂\\\"\n".repeat(20_000);
            let value = if source_text {
                serde_json::json!(decoded)
            } else {
                serde_json::json!({"arg": decoded})
            };
            let raw = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &raw).unwrap();
            let mut guard = cv_core::source::ReadSet::default();
            guard.record(&conditional);
            let input = cv_core::json::Json::source_with_reads(&path, 0, raw.len() as u64, &guard).unwrap();
            let block = if source_text {
                cv_core::Block::Text {
                    text: input.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap(),
                }
            } else {
                cv_core::Block::ToolUse {
                    id: "call".into(),
                    name: "tool".into(),
                    input,
                    namespace: None,
                }
            };
            let expected = if source_text {
                format!("{decoded}\n")
            } else {
                format!("tool {value}\n")
            };
            let mut bodies = Vec::new();
            let mut out = |doc: TantivyDocument| -> Result<()> {
                if let Some(body) = doc.get_first(f.body).and_then(|v| v.as_str()) {
                    assert!(body.len() <= 2 * CHUNK_BYTES);
                    bodies.push(body.to_owned());
                }
                Ok(())
            };
            let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
            sink.append_block(&cv_core::Resolver::new(None), &block);
            assert!(sink.buf.capacity() <= 2 * CHUNK_BYTES);
            sink.finish().unwrap();
            let stored: cv_core::source::ReadSet =
                serde_json::from_slice(&serde_json::to_vec(&sink.dependencies).unwrap()).unwrap();
            drop(sink);
            assert_eq!(bodies.concat(), expected);
            assert!(stored.fresh(&mut cv_core::source::Probe::default()));
            // Primary append identity is separately tracked; native source ranges must not
            // turn the auxiliary guard into a whole-primary append-reuse veto.
            std::fs::write(&path, "primary changed").unwrap();
            assert!(stored.fresh(&mut cv_core::source::Probe::default()));
            std::fs::write(&conditional, "conditional appeared or changed").unwrap();
            assert!(!stored.fresh(&mut cv_core::source::Probe::default()));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_text_mutation_during_successful_or_failed_flush_stays_failed() {
        use cv_core::MessageSink;
        let dir = tmpdir();
        let path = dir.join("source.json");
        let conditional = dir.join("conditional");
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("source", cv_core::Harness::Claude, dir.join("primary"));
        for fail_output in [false, true] {
            for changed in [&path, &conditional] {
                let raw = serde_json::to_vec(&"zqsourcecarrier ".repeat(2 * CHUNK_BYTES)).unwrap();
                std::fs::write(&path, &raw).unwrap();
                let mut guard = cv_core::source::ReadSet::default();
                guard.record(&conditional);
                let input = cv_core::json::Json::source_with_reads(&path, 0, raw.len() as u64, &guard).unwrap();
                let text = input.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
                let mut emitted = 0;
                let mut out = |_doc: TantivyDocument| -> Result<()> {
                    emitted += 1;
                    std::fs::write(changed, format!("mutation {fail_output} {emitted}"))?;
                    anyhow::ensure!(!fail_output, "deliberate output failure");
                    Ok(())
                };
                let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
                sink.append_text(&cv_core::Resolver::new(None), &text);
                assert!(sink.take_error().is_some());
                assert_eq!(
                    sink.message(cv_core::Message::new(cv_core::Role::User)),
                    cv_core::Flow::Stop
                );
                assert!(sink.finish().is_err());
                drop(sink);
                assert!(emitted > 0);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn demanded_text_source_mutation_latches_failure_after_bounded_flush() {
        use cv_core::MessageSink;
        let dir = tmpdir();
        let path = dir.join("text");
        let text = "zqtextmutation ".repeat(2 * CHUNK_BYTES);
        let mut source = " ".repeat(8192);
        source.push_str(&text);
        std::fs::write(&path, &source).unwrap();
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("text", cv_core::Harness::Claude, dir.join("primary.jsonl"));
        let mut emitted = 0;
        let mut out = |doc: TantivyDocument| -> Result<()> {
            emitted += 1;
            assert!(doc
                .get_first(f.body)
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.len() <= 2 * CHUNK_BYTES));
            std::fs::write(&path, "changed during flush")?;
            Ok(())
        };
        let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
        sink.push_chunk("prefix\n");
        sink.append_text(
            &cv_core::Resolver::new(None),
            &cv_core::Text::Span(cv_core::Span {
                source: Some(path.clone()),
                offset: 8192,
                len: text.len() as u64,
                escaped: false,
            }),
        );
        assert!(sink.take_error().is_some());
        let mut later = cv_core::Message::new(cv_core::Role::User);
        later.content.push(cv_core::Block::Text { text: "later".into() });
        assert!(matches!(sink.message(later), cv_core::Flow::Stop));
        assert!(sink.finish().is_err());
        drop(sink);
        assert!(emitted > 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn demanded_text_malformed_escape_fallback_matches_existing_body_and_bounds() {
        let dir = tmpdir();
        let path = dir.join("text");
        let raw = "before\\q\\uD800after ".repeat(CHUNK_BYTES);
        std::fs::write(&path, &raw).unwrap();
        let span = cv_core::Span {
            source: Some(path),
            offset: 0,
            len: raw.len() as u64,
            escaped: true,
        };
        let resolver = cv_core::Resolver::new(None);
        let expected = resolver.resolve(&span).into_owned();
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("fallback", cv_core::Harness::Claude, dir.join("primary.jsonl"));
        let mut bodies = Vec::new();
        let mut out = |doc: TantivyDocument| -> Result<()> {
            if let Some(body) = doc.get_first(f.body).and_then(|v| v.as_str()) {
                assert!(body.len() <= 2 * CHUNK_BYTES);
                bodies.push(body.to_owned());
            }
            Ok(())
        };
        let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
        sink.append_text(&resolver, &cv_core::Text::Span(span));
        assert!(sink.buf.capacity() <= 2 * CHUNK_BYTES);
        sink.finish().unwrap();
        drop(sink);
        assert_eq!(bodies.concat(), expected);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tool_input_source_json_matches_canonical_body_and_reports_staleness() {
        let dir = std::env::temp_dir().join(format!("cv-fts-json-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("input.json");
        let raw = format!("{{\"arg\":\"{}\",\"tail\":true}}", "\\u00e9\\n\\\"".repeat(CHUNK_BYTES));
        std::fs::write(&path, &raw).unwrap();
        let input = cv_core::json::Json::source(&path, 0, raw.len() as u64).unwrap();
        let expected = format!("tool {}\n", serde_json::from_str::<serde_json::Value>(&raw).unwrap());
        let block = cv_core::Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            input,
            namespace: None,
        };
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("input", cv_core::Harness::Claude, &path);
        let mut bodies = Vec::new();
        let mut out = |doc: TantivyDocument| -> Result<()> {
            if let Some(body) = doc.get_first(f.body).and_then(|value| value.as_str()) {
                bodies.push(body.to_owned());
            }
            Ok(())
        };
        let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
        sink.append_block(&cv_core::Resolver::new(None), &block);
        sink.finish().unwrap();
        drop(sink);
        assert_eq!(bodies.concat(), expected);
        std::fs::write(&path, "null").unwrap();
        let mut ignore = |_doc: TantivyDocument| -> Result<()> { Ok(()) };
        let mut sink = ChunkSink::new(&mut ignore, &f, &r, 1, 1, Default::default());
        sink.append_block(&cv_core::Resolver::new(None), &block);
        assert!(format!("{:#}", sink.finish().unwrap_err()).contains("JSON source changed"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tool_input_json_streaming_retains_document_output_error() {
        let f = fields_of(&build_schema()).unwrap();
        let r = cv_core::SessionRef::new("input", cv_core::Harness::Claude, "/absent.jsonl");
        let mut emitted = 0;
        let mut out = |_doc: TantivyDocument| -> Result<()> {
            emitted += 1;
            anyhow::bail!("test document output closed")
        };
        let mut sink = ChunkSink::new(&mut out, &f, &r, 1, 1, Default::default());
        sink.append_block(
            &cv_core::Resolver::new(None),
            &cv_core::Block::ToolUse {
                id: "call".into(),
                name: "Edit".into(),
                input: serde_json::json!({"content":"x".repeat(8 * CHUNK_BYTES)}).into(),
                namespace: None,
            },
        );
        assert!(sink
            .finish()
            .unwrap_err()
            .to_string()
            .contains("test document output closed"));
        drop(sink);
        assert_eq!(emitted, 1);
    }

    /// Run alone under /usr/bin/time -l with frozen scratch fixtures; emits and drops real FTS
    /// documents before Tantivy tokenization so source/tee/chunk demand is measured separately.
    #[test]
    #[ignore = "explicit scratch CV_FTS_INPUT_PROBE and CV_FTS_INPUT_BYTES required"]
    fn tool_input_fts_demand_probe() {
        let path = std::fs::canonicalize(std::env::var_os("CV_FTS_INPUT_PROBE").unwrap()).unwrap();
        let input_bytes: usize = std::env::var("CV_FTS_INPUT_BYTES").unwrap().parse().unwrap();
        let _home = IsolatedHome::new();
        let mut r = cv_core::SessionRef::new("perf", cv_core::Harness::Claude, &path);
        r.cwd = Some("/perf".into());
        let source = cv_core::source::Probe::default().snapshot(&r);
        let adapter = cv_core::harness::claude::Claude::with_config_dir(path.parent().unwrap());
        let fields = fields_of(&build_schema()).unwrap();
        fn hash(mut hash: u64, bytes: &[u8]) -> u64 {
            for &byte in bytes {
                hash = (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211);
            }
            hash
        }
        let initial = 14_695_981_039_346_656_037;
        let mut digest = initial;
        let mut body_bytes = 0;
        let mut largest_body = 0;
        let mut body_docs = 0;
        let mut emit = |doc: TantivyDocument| -> Result<()> {
            if let Some(body) = doc.get_first(fields.body).and_then(|value| value.as_str()) {
                digest = hash(digest, body.as_bytes());
                body_bytes += body.len();
                largest_body = largest_body.max(body.len());
                body_docs += 1;
            }
            Ok(())
        };
        let start = std::time::Instant::now();
        let mut events = cv_core::events::SpoolingSink::new(r.cwd.clone());
        let (_, done, _) = index_session(
            &mut emit,
            &fields,
            &adapter,
            &r,
            source.primary.0,
            source.primary.1,
            Some(&mut events),
            Default::default(),
            None,
        )
        .unwrap()
        .unwrap();
        let event_source = source.with_reads(events.inner().dependencies());
        let spool = events.into_spool().unwrap();
        assert_eq!(spool.len(), 1);
        let counted = done.counted.unwrap();
        assert_eq!(counted.turns, 2);
        let mut writer = cv_core::catalog::Writer::open().unwrap();
        writer
            .record_events(&r, 0, spool, &event_source, &Default::default())
            .unwrap();
        writer.record_counted(&r, &source, counted);
        drop(writer);
        let wall = start.elapsed().as_secs_f64();
        let rows = cv_core::events::events_for("claude", "perf", None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "file_edit");
        assert_eq!(rows[0].target.as_deref(), Some("/perf/a.rs"));
        let prefix = b"Inspect the file\nEdit {\"file_path\":\"/perf/a.rs\",\"content\":\"";
        let suffix = b"\",\"metadata\":{\"n\":3,\"flag\":true}}\n";
        let mut expected = hash(initial, prefix);
        let x = [b'x'; 4096];
        for _ in 0..input_bytes / x.len() {
            expected = hash(expected, &x);
        }
        expected = hash(expected, &x[..input_bytes % x.len()]);
        expected = hash(expected, suffix);
        assert_eq!(digest, expected, "canonical searchable JSON must be exact");
        assert_eq!(body_bytes, prefix.len() + input_bytes + suffix.len());
        eprintln!(
            "{}",
            serde_json::json!({"input_bytes":input_bytes,"body_bytes":body_bytes,"body_docs":body_docs,"largest_body_bytes":largest_body,"wall_seconds":wall,"body_hash":format!("{digest:016x}"),"events":rows.len(),"chunk_bytes":CHUNK_BYTES,"tantivy_tokenization":false})
        );
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cv-search-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Indexing tees events into the catalog, whose path comes from the process-global
    /// `CLUSTERVISION_HOME`. Tests that index MUST isolate it to a temp dir — otherwise they write to
    /// (and read from) the shared real catalog, racing each other and the other crates' test binaries.
    /// This RAII guard serializes those tests on one lock AND points the catalog at a private temp
    /// home for the test's duration; both are released (and the home removed) on drop.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct IsolatedHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        home: std::path::PathBuf,
    }
    impl IsolatedHome {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let home = tmpdir();
            std::env::set_var("CLUSTERVISION_HOME", &home);
            IsolatedHome { _lock: lock, home }
        }
    }
    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            std::env::remove_var("CLUSTERVISION_HOME");
            std::fs::remove_dir_all(&self.home).ok();
        }
    }

    /// Write a minimal one-message Claude transcript so `live_snippet` has a real file to re-read,
    /// and return its path. `body` becomes the single user message's text.
    fn write_claude(dir: &Path, id: &str, body: &str) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": id,
            "sessionId": id,
            "message": { "role": "user", "content": body }
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p.display().to_string()
    }

    /// A [`cv_core::SessionRef`] pointing at a written Claude transcript, with the dates the index is
    /// expected to surface (`created_at`→1s, `updated_at`→2s, as the hit assertions check). The
    /// production `ChunkSink` reads these straight off the ref — Claude's adapter never emits a
    /// metadata title, so `title` here is the authoritative label.
    fn sref(id: &str, title: &str, path: String) -> cv_core::SessionRef {
        cv_core::SessionRef {
            id: id.into(),
            harness: cv_core::Harness::Claude,
            path: path.into(),
            cwd: Some("/home/u/proj".into()),
            title: Some(title.into()),
            created_at: chrono::DateTime::from_timestamp(1, 0),
            updated_at: chrono::DateTime::from_timestamp(2, 0),
            message_count: 1,
        }
    }

    /// Like [`sref`] but with no discovery-time title — so the indexed label falls back to the first
    /// user message's text (the path Claude sessions without an `ai-title` line take).
    fn sref_untitled(id: &str, path: String) -> cv_core::SessionRef {
        cv_core::SessionRef {
            title: None,
            ..sref(id, "", path)
        }
    }

    /// Write a multi-message Claude transcript (one JSONL record per `(role, content)`), returning its
    /// path. `role` is `"user"` or `"assistant"`; content is a bare string.
    fn write_claude_msgs(dir: &Path, id: &str, msgs: &[(&str, &str)]) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let mut out = String::new();
        for (i, (role, content)) in msgs.iter().enumerate() {
            let line = serde_json::json!({
                "type": role,
                "uuid": format!("{id}-{i}"),
                "sessionId": id,
                "message": { "role": role, "content": content }
            });
            out.push_str(&line.to_string());
            out.push('\n');
        }
        std::fs::write(&p, out).unwrap();
        p.display().to_string()
    }

    #[test]
    fn indexed_matching_reads_no_sources_and_keeps_all_chunk_matches() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let path = write_claude(&sdir, "chunked", "alpha beta originalbody");
        let refs = vec![sref("chunked", "old title", path.clone())];
        index_refs(&dir, &refs, false).unwrap();
        let before = text_search_indexed(&dir, "alpha", 10).unwrap();
        std::fs::remove_file(&path).unwrap();
        let after = text_search_indexed(&dir, "alpha", 10).unwrap();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        assert_eq!(
            text_search_ids(&dir, "\"alpha beta\" harness:claude").unwrap(),
            HashSet::from(["chunked".to_string()])
        );
        assert!(text_search_ids(&dir, "alpha nonexistent").unwrap().is_empty());
        assert!(text_search_ids(&dir, "harness:codex alpha").unwrap().is_empty());
        assert!(text_search_ids(&dir, "\"").is_err());
        let needles = [
            "alpha",
            "\"",
            "alpha nonexistent",
            "\"alpha beta\" harness:claude",
            "harness:codex alpha",
        ];
        let batch = text_search_ids_batch(&dir, &needles).unwrap();
        for (query, result) in needles.iter().zip(batch) {
            match (text_search_ids(&dir, query), result) {
                (Ok(single), Ok(batched)) => assert_eq!(single, batched),
                (Err(single), Err(batched)) => assert_eq!(single.to_string(), batched.to_string()),
                _ => panic!("batch/single query mismatch: {query}"),
            }
        }
        let absent = dir.join("no-index");
        assert!(text_search_ids_batch(&absent, &[]).unwrap().is_empty());
        assert!(text_search_ids_batch(&absent, &["alpha"]).is_err());

        // Many matching chunks and unique sessions: membership enumerates the entire match set.
        let (index, f) = open_existing(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        for _ in 0..300 {
            let mut doc = TantivyDocument::default();
            doc.add_text(f.id, "chunked");
            doc.add_text(f.body, "allmatches");
            writer.add_document(doc).unwrap();
        }
        let mut expected = HashSet::from(["chunked".to_string()]);
        for i in 0..130 {
            let id = format!("other-{i}");
            let mut doc = TantivyDocument::default();
            doc.add_text(f.id, &id);
            doc.add_text(f.body, "allmatches");
            writer.add_document(doc).unwrap();
            expected.insert(id);
        }
        writer.commit().unwrap();
        assert_eq!(text_search_ids(&dir, "allmatches").unwrap(), expected);
        drop(writer);
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(sdir).ok();
    }

    /// Run in an isolated process. Prepare once, then compare full/head modes under `/usr/bin/time`.
    #[test]
    #[ignore = "manual CPU and docstore I/O probe"]
    fn session_identity_lookup_probe() {
        let dir = PathBuf::from(std::env::var_os("CV_SEARCH_ID_PROBE_DIR").expect("isolated probe dir"));
        let mode = std::env::var("CV_SEARCH_ID_PROBE_MODE").unwrap_or_else(|_| "head".into());
        if !dir.join("meta.json").exists() {
            assert_eq!(mode, "prepare", "prepare an isolated fixture before benchmarking");
            let (index, f) = open_or_create(&dir).unwrap();
            let mut writer = open_writer(&index, 15_000_000).unwrap();
            for i in 0..32_000 {
                writer.add_document(tantivy::doc!(
                    f.id => format!("session-{:04}", i / 64), f.kind => KIND_BODY,
                    f.harness => "claude", f.cwd => "/Users/example/dev/project/subdirectory/".repeat(3),
                    f.title => "Representative session label describing a coding task and its context",
                    f.path => format!("/Users/example/.claude/projects/example-project/session-{:04}.jsonl", i / 64),
                    f.created_at => 123i64, f.first_msg => i as i64,
                    f.body => "needle identical query body"
                )).unwrap();
                if i % 8_000 == 7_999 {
                    writer.commit().unwrap();
                }
            }
            // Giant searchable text cannot enter the docstore: only its tiny metadata is stored.
            writer.add_document(tantivy::doc!(f.id => "giant", f.kind => KIND_BODY, f.body => format!("giantproof {}", "z".repeat(8 << 20)))).unwrap();
            writer.commit().unwrap();
        }
        let (index, f) = open_existing(&dir).unwrap();
        let searcher = index.reader().unwrap().searcher();
        if mode == "prepare" {
            let mut logical_bytes = 0;
            let mut store_bytes = 0;
            for segment in searcher.segment_readers() {
                let store = segment.get_store_reader(1).unwrap();
                store_bytes += store.space_usage().total().get_bytes();
                for doc in 0..segment.max_doc() {
                    logical_bytes += store.get_document_bytes(doc).unwrap().len();
                }
            }
            let giant = parse_text_query(&index, &f, "giantproof").unwrap();
            let addr = searcher
                .search(&giant, &TopDocs::with_limit(1).order_by_score())
                .unwrap()[0]
                .1;
            let doc: TantivyDocument = searcher.doc(addr).unwrap();
            assert!(doc.get_first(f.body).is_none());
            let store = searcher.segment_reader(addr.segment_ord).get_store_reader(1).unwrap();
            let giant_stored_bytes = store.get_document_bytes(addr.doc_id).unwrap().len();
            println!("probe docs={} logical_stored_bytes={logical_bytes} compressed_store_bytes={store_bytes} giant_body_bytes={} giant_stored_bytes={giant_stored_bytes} fast_fields={:?}", searcher.num_docs(), 8 << 20,
                index.schema().fields().filter(|(_, field)| field.is_fast()).map(|(_, field)| field.name().to_owned()).collect::<Vec<_>>());
            return;
        }
        let query_text = std::env::var("CV_SEARCH_ID_PROBE_QUERY").unwrap_or_else(|_| "needle".into());
        let query = parse_text_query(&index, &f, &query_text).unwrap();
        let matched = searcher.search(&query, &tantivy::collector::Count).unwrap();
        let mut full = crate::session_collector::TopSessions::new(f.id, 20);
        full.full_documents = true;
        let head = crate::session_collector::TopSessions::new(f.id, 20);
        let expected = searcher.search(&query, &full).unwrap();
        assert_eq!(searcher.search(&query, &head).unwrap(), expected);
        let collector = if mode == "full" { &full } else { &head };
        let iterations: usize = std::env::var("CV_SEARCH_ID_PROBE_REPEATS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            assert_eq!(searcher.search(&query, collector).unwrap(), expected);
        }
        println!(
            "probe mode={mode} query={query_text:?} queries={iterations} matching_documents={matched} returned_sessions={} limit=20 elapsed_ms={}",
            expected.len(),
            start.elapsed().as_millis()
        );
    }

    #[test]
    fn ranked_sessions_survive_dominant_chunks_and_cutoff_ties() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        // A hundred best-scoring chunks from one session used to fill the whole fetch window.
        for _ in 0..100 {
            writer.add_document(tantivy::doc!(f.id => "dominant", f.body => "needle needle needle", f.preview => "dominant preview")).unwrap();
        }
        // Reverse id order exercises equal-score cutoff selection beyond the former 64 docs.
        for i in (0..100).rev() {
            writer.add_document(tantivy::doc!(f.id => format!("other-{i:03}"), f.body => "needle padding padding padding", f.preview => "other preview")).unwrap();
        }
        writer.commit().unwrap();
        // The same sessions span another segment, including a better chunk of other-099.
        writer
            .add_document(tantivy::doc!(f.id => "other-099", f.body => "needle needle needle needle"))
            .unwrap();
        writer
            .add_document(tantivy::doc!(f.id => "dominant", f.body => "needle padding padding padding"))
            .unwrap();
        writer.commit().unwrap();
        writer.delete_term(Term::from_field_text(f.id, "other-050"));
        writer.commit().unwrap();
        drop(writer);

        // Exhaustive chunk oracle establishes both scores and best-document selection without
        // sharing the bounded collector implementation.
        let searcher = index.reader().unwrap().searcher();
        let query = parse_text_query(&index, &f, "needle").unwrap();
        let mut all: Vec<_> = searcher
            .search(
                &query,
                &TopDocs::with_limit(searcher.num_docs() as usize).order_by_score(),
            )
            .unwrap()
            .into_iter()
            .map(|(score, addr)| {
                let doc: TantivyDocument = searcher.doc(addr).unwrap();
                (score, doc.get_first(f.id).unwrap().as_str().unwrap().to_owned())
            })
            .collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut seen = HashSet::new();
        all.retain(|(_, id)| seen.insert(id.clone()));
        for limit in [0, 1, 2, 3, 10, 65, 100, 500] {
            let hits = text_search_indexed(&dir, "needle", limit).unwrap();
            let actual: Vec<_> = hits.into_iter().map(|h| (h.score, h.id)).collect();
            assert_eq!(
                actual,
                all.iter().take(limit).cloned().collect::<Vec<_>>(),
                "limit={limit}"
            );
        }
        assert_eq!(
            text_search_indexed(&dir, "needle", 3).unwrap()[1].snippet,
            "dominant preview"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    // Frozen pre-cache implementation: keeps the old segment-local lookup behavior as oracle.
    fn legacy_ranked_search(
        dir: &Path,
        query: &str,
        limit: usize,
        filter: crate::SearchFilter<'_>,
        queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<Vec<Hit>> {
        if !dir.join("meta.json").exists() {
            anyhow::bail!("no full-text index at {} — run `index_all` first", dir.display());
        }
        // Read-only open: a transient failure here must surface as an error, never as a wiped index.
        let (index, f) = open_existing(dir)?;
        let reader = index.reader().context("opening index reader")?;
        let searcher = reader.searcher();

        let parsed = parse_text_query(&index, &f, query)?;

        // Collect sessions rather than chunks. Each segment's top `limit` sessions contains every
        // possible global winner, even when one session has arbitrarily many matching chunks.
        let mut collector = crate::session_collector::TopSessions::new(f.id, limit);
        if filter.harness.is_some() || filter.cwd.is_some() {
            let harness = filter.harness.map(str::to_owned);
            let cwd = filter.cwd.map(str::to_owned);
            let fields = f.clone();
            let reader = searcher.clone();
            let queries = queries.clone();
            collector.filter = Some(std::sync::Arc::new(move |id, store, doc_id| {
                // Resumed chunks can retain older labels/cwd: filter the same current meta doc that
                // describes the returned hit, falling back only for legacy body-only documents.
                queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let meta = meta_doc(&reader, &fields, id);
                let fallback;
                let doc = if let Some(meta) = meta.as_ref() {
                    meta
                } else {
                    fallback = store.get::<TantivyDocument>(doc_id)?;
                    &fallback
                };
                Ok(crate::session_collector::FilterDecision::Session(
                    harness.as_ref().is_none_or(|wanted| {
                        doc.get_first(fields.harness).and_then(|v| v.as_str()) == Some(wanted.as_str())
                    }) && cwd.as_ref().is_none_or(|needle| {
                        doc.get_first(fields.cwd)
                            .and_then(|v| v.as_str())
                            .is_some_and(|value| value.contains(needle))
                    }),
                ))
            }));
        }
        let ranked = searcher.search(&parsed, &collector).context("running search")?;

        let mut hits = Vec::with_capacity(ranked.len());
        for candidate in ranked {
            let (score, id) = (candidate.score, candidate.id);
            let hit_doc: TantivyDocument = searcher.doc(candidate.addr).context("loading stored doc")?;
            // What describes the session — its current label, dates, preview — is on its meta doc; a
            // body doc carries the label it was written under.
            let is_meta = hit_doc.get_first(f.kind).and_then(|v| v.as_str()) == Some(KIND_META);
            let doc = if is_meta {
                hit_doc
            } else {
                queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                meta_doc(&searcher, &f, &id).unwrap_or(hit_doc)
            };
            let get_str = |field: Field| -> Option<String> {
                doc.get_first(field).and_then(|v| v.as_str()).map(|s| s.to_string())
            };
            let harness = get_str(f.harness).unwrap_or_default();
            let snippet = get_str(f.preview).unwrap_or_default();

            hits.push(Hit {
                id,
                harness,
                cwd: get_str(f.cwd),
                title: get_str(f.title),
                created_at: doc.get_first(f.created_at).and_then(|v| v.as_i64()),
                updated_at: doc.get_first(f.updated_at).and_then(|v| v.as_i64()),
                score,
                snippet,
                parent_id: get_str(f.parent_id),
                agent_id: get_str(f.agent_id),
                workflow: get_str(f.workflow),
            });
        }
        Ok(hits)
    }

    #[test]
    fn invalid_query_errors_remain_distinct_from_missing_index_and_empty_results() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        open_or_create(&dir).unwrap();
        let ranked = text_search_indexed(&dir, "no_such_field:value", 1).unwrap_err();
        assert!(crate::is_invalid_text_query(&ranked));
        let batch = text_search_ids_batch(&dir, &["needle", "no_such_field:value"]).unwrap();
        assert!(batch[0].as_ref().unwrap().is_empty());
        assert!(crate::is_invalid_text_query(batch[1].as_ref().unwrap_err()));
        let setup = text_search_ids(&dir.join("missing"), "needle").unwrap_err();
        assert!(!crate::is_invalid_text_query(&setup));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn body_only_filter_decisions_are_not_reused_by_session_identity() {
        let _home = IsolatedHome::new();
        for reverse in [false, true] {
            let dir = tmpdir();
            let (index, f) = open_or_create(&dir).unwrap();
            let mut writer = open_writer(&index, 15_000_000).unwrap();
            let rows = [
                ("claude", "/outside", "needle needle needle", "wrong"),
                ("codex", "/wanted", "needle padding padding", "right"),
            ];
            for i in if reverse { vec![1, 0] } else { vec![0, 1] } {
                let (harness, cwd, body, preview) = rows[i];
                writer
                    .add_document(tantivy::doc!(f.id => "legacy", f.kind => KIND_BODY,
                    f.harness => harness, f.cwd => cwd, f.body => body, f.preview => preview))
                    .unwrap();
            }
            writer.commit().unwrap();
            drop(writer);
            for filter in [
                crate::SearchFilter {
                    harness: Some("codex"),
                    cwd: None,
                },
                crate::SearchFilter {
                    harness: None,
                    cwd: Some("wanted"),
                },
            ] {
                let hits = text_search_indexed_filtered(&dir, "needle", 1, filter).unwrap();
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].harness, "codex");
                assert_eq!(hits[0].snippet, "right");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn winner_header_projection_preserves_meta_and_legacy_fallback() {
        use std::sync::{atomic::AtomicUsize, Arc};
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        for doc in [
            tantivy::doc!(f.id => "current", f.kind => KIND_BODY, f.body => "needle", f.preview => "discarded".repeat(131072)),
            tantivy::doc!(f.id => "current", f.kind => KIND_META, f.preview => "current preview", f.title => "current title"),
            // Compatible documents place kind after the inspected header or omit it entirely.
            tantivy::doc!(f.title => "reordered", f.id => "legacy", f.kind => KIND_BODY, f.body => "needle", f.preview => "legacy preview"),
            tantivy::doc!(f.id => "missing-kind", f.title => "old", f.body => "needle", f.preview => "old preview"),
            // Candidate metadata rows must not be replaced by another same-id metadata row.
            tantivy::doc!(f.id => "duplicate-meta", f.kind => KIND_META, f.title => "needle", f.preview => "matching meta"),
            tantivy::doc!(f.id => "duplicate-meta", f.kind => KIND_META, f.title => "unrelated", f.preview => "other meta"),
        ] {
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        drop(writer);
        let searcher = index.reader().unwrap().searcher();
        let filter = crate::SearchFilter::default();
        let expected = legacy_ranked_search(&dir, "needle", 10, filter, Arc::new(AtomicUsize::new(0))).unwrap();
        let decisions = Arc::new(MetadataDecisions::new(&searcher, &f, filter));
        let actual = ranked_search(&index, &f, &searcher, "needle", 10, filter, decisions).unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(
            actual.iter().find(|hit| hit.id == "current").unwrap().snippet,
            "current preview"
        );
        assert_eq!(
            actual.iter().find(|hit| hit.id == "duplicate-meta").unwrap().snippet,
            "matching meta"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ranked_metadata_work_is_shared_across_many_segments() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        for i in 0..128 {
            let accepted = i < 4;
            writer
                .add_document(tantivy::doc!(f.id => format!("session-{i:03}"), f.kind => KIND_META,
                f.harness => if accepted { "codex" } else { "claude" },
                f.cwd => if accepted { "/current/project" } else { "/outside" },
                f.title => format!("current title {i}"), f.preview => format!("current preview {i}"),
                f.created_at => 11i64, f.updated_at => 22i64))
                .unwrap();
        }
        writer.commit().unwrap();
        for segment in 0..16 {
            for i in 0..128 {
                writer
                    .add_document(tantivy::doc!(f.id => format!("session-{i:03}"), f.kind => KIND_BODY,
                    f.body => if i < 4 { "needle padding padding" } else { "needle needle needle" },
                    f.harness => "old-harness", f.cwd => "/stale", f.preview => "stale preview"))
                    .unwrap();
            }
            // Missing metadata must not share one segment's body decision with another segment.
            writer.add_document(tantivy::doc!(f.id => "legacy", f.kind => KIND_BODY,
                f.body => "needle padding padding padding", f.harness => if segment % 2 == 0 { "codex" } else { "claude" },
                f.cwd => "/current/project", f.preview => "legacy preview")).unwrap();
            writer.commit().unwrap();
        }
        drop(writer);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 17);
        let filter = crate::SearchFilter {
            harness: Some("codex"),
            cwd: Some("current/proj"),
        };
        let legacy_queries = Arc::new(AtomicUsize::new(0));
        let expected = legacy_ranked_search(&dir, "needle", 10, filter, legacy_queries.clone()).unwrap();
        let decisions = Arc::new(MetadataDecisions::new(&searcher, &f, filter));
        let actual = ranked_search(&index, &f, &searcher, "needle", 10, filter, decisions.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(actual.len(), 5);
        assert!(actual.iter().any(|hit| hit.id == "legacy"));
        let queries = decisions.queries.load(Ordering::Relaxed);
        let reads = decisions.stored_reads.load(Ordering::Relaxed);
        assert_eq!(queries, 129);
        assert_eq!(reads, 128);
        assert_eq!(decisions.winner_body_reads.load(Ordering::Relaxed), 1);
        assert_eq!(decisions.winner_metadata_reads.load(Ordering::Relaxed), 4);
        assert_eq!(decisions.cache.lock().unwrap().len(), 129);
        assert_eq!(legacy_queries.load(Ordering::Relaxed), 16 * 129 + 5);
        // A top-three cutoff still compares exact scores/ties and current stored previews.
        let top = text_search_indexed_filtered(&dir, "needle", 3, filter).unwrap();
        assert_eq!(
            serde_json::to_value(top).unwrap(),
            serde_json::to_value(&expected[..3]).unwrap()
        );
        assert!(text_search_indexed_filtered(
            &dir,
            "needle",
            3,
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: Some("CURRENT")
            }
        )
        .unwrap()
        .is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ranked_sessions_filter_before_cutoff_using_current_metadata() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        for i in 0..100 {
            writer.add_document(tantivy::doc!(f.id => format!("unwanted-{i}"), f.body => "needle needle needle", f.harness => "claude", f.cwd => "/outside/project")).unwrap();
        }
        writer.add_document(tantivy::doc!(f.id => "wanted", f.kind => KIND_BODY, f.body => "needle padding padding", f.harness => "codex", f.cwd => "/stale/path")).unwrap();
        writer.add_document(tantivy::doc!(f.id => "wanted", f.kind => KIND_META, f.harness => "codex", f.cwd => "/repo/mixed.Path/project", f.preview => "current preview")).unwrap();
        writer.commit().unwrap();
        drop(writer);
        for filter in [
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: None,
            },
            crate::SearchFilter {
                harness: None,
                cwd: Some("mixed.Path/proj"),
            },
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: Some("mixed.Path/proj"),
            },
        ] {
            let hits = text_search_indexed_filtered(&dir, "needle", 1, filter).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, "wanted");
            assert_eq!(hits[0].snippet, "current preview");
        }
        for cwd in ["MIXED", "/stale/path"] {
            assert!(text_search_indexed_filtered(
                &dir,
                "needle",
                1,
                crate::SearchFilter {
                    harness: None,
                    cwd: Some(cwd)
                }
            )
            .unwrap()
            .is_empty());
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn live_snippets_are_enriched_only_for_selected_hits() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let refs: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|id| sref(id, "common title", write_claude(&sdir, id, "common originalbody")))
            .collect();
        index_refs(&dir, &refs, false).unwrap();
        let mut hits = text_search_indexed(&dir, "common", 10).unwrap();
        assert_eq!(hits.len(), 2);
        let untouched = hits[1].snippet.clone();
        for r in &refs {
            write_claude(&sdir, &r.id, "common replacementbody");
        }
        enrich_text_snippets(&dir, "common", &mut hits[..1]).unwrap();
        assert!(hits[0].snippet.contains("replacementbody"));
        assert_eq!(hits[1].snippet, untouched);
        assert!(hits[1].snippet.contains("originalbody"));
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(sdir).ok();
    }

    #[test]
    fn index_and_search_with_live_snippets() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let b1 = "We built a full text search engine with tantivy and BM25 scoring.";
        let b2 = "Loading dataframes and grouping by columns in pandas.";
        let p1 = write_claude(&sdir, "a1", b1);
        let p2 = write_claude(&sdir, "b2", b2);
        let refs = vec![sref("a1", "Rust tantivy index", p1), sref("b2", "Python pandas", p2)];
        let n = index_refs(&dir, &refs, false).unwrap();
        assert_eq!(n, 2);

        // Term unique to the first doc; snippet is generated live from the session file.
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected exactly one match for 'tantivy'");
        assert_eq!(hits[0].id, "a1");
        assert!(
            hits[0].snippet.contains("tantivy"),
            "live snippet should mention the term: {:?}",
            hits[0].snippet
        );
        // Dates come straight off the index now (no corpus re-discovery).
        assert_eq!(hits[0].created_at, Some(1));
        assert_eq!(hits[0].updated_at, Some(2));

        let hits = text_search(&dir, "pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b2");

        // Fielded query restricts by harness.
        let hits = text_search(&dir, "harness:claude pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].harness, "claude");

        // Conjunction-by-default: a term present in neither yields nothing.
        assert!(text_search(&dir, "kubernetes", 10).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn persisted_tool_output_files_are_indexed_and_snippeted() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // Claude Code parks a too-large tool output in `<session>/tool-results/<id>.txt` and leaves
        // only a stub (pointer + preview) in the transcript. The marker word exists ONLY in the file.
        let tr = sdir.join("p1").join("tool-results");
        std::fs::create_dir_all(&tr).unwrap();
        let file = tr.join("abc123.txt");
        std::fs::write(
            &file,
            "line one\nthe zanzibar constant appears only on disk\nline three\n",
        )
        .unwrap();
        let stub = format!(
            "<persisted-output>\nOutput too large (66KB). Full output saved to: {}\n\nPreview (first 2KB):\nline one\n</persisted-output>",
            file.display()
        );
        let p = sdir.join("p1.jsonl");
        let lines = [
            serde_json::json!({"type":"user","uuid":"u0","sessionId":"p1","timestamp":"2026-09-19T00:00:00Z",
                "message":{"role":"user","content":"run the harvester"}}),
            serde_json::json!({"type":"assistant","uuid":"a0","parentUuid":"u0","sessionId":"p1","timestamp":"2026-09-19T00:00:01Z",
                "message":{"role":"assistant","model":"m","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"harvest"}}]}}),
            serde_json::json!({"type":"user","uuid":"u1","parentUuid":"a0","sessionId":"p1","timestamp":"2026-09-19T00:00:02Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":stub}]}}),
        ];
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        index_refs(&dir, &[sref("p1", "harvest run", p.display().to_string())], false).unwrap();

        let hits = text_search(&dir, "zanzibar", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "a word that lives only in the persisted file is findable"
        );
        assert_eq!(hits[0].id, "p1");
        assert!(
            hits[0].snippet.contains("zanzibar"),
            "the live snippet scans the file too: {:?}",
            hits[0].snippet
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn adopted_output_read_sets_invalidate_search_and_survive_resume() {
        let _home = IsolatedHome::new();
        let (dir, sdir, external) = (tmpdir(), tmpdir(), tmpdir());
        // Adoption retains an absolute pointer into a different parent's directory.
        let file = external.join("output.txt");
        let stub = format!(
            "<persisted-output>\nFull output saved to: {}\n\nPreview (first 2KB):\npreview\n</persisted-output>",
            file.display()
        );
        let p = sdir.join("adopted.jsonl");
        let mut lines = vec![serde_json::json!({"type":"user","uuid":"tool","sessionId":"adopted",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":stub}]}})];
        for i in 0..60 {
            lines.push(
                serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"adopted",
                "message":{"role":"user","content":"body ".repeat(100)}}),
            );
        }
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        let r = sref("adopted", "adopted", p.display().to_string());
        let refresh = || sweep(&dir, vec![r.clone()], false, false).unwrap();
        std::fs::write(&file, "zanzibar").unwrap();
        assert_eq!(refresh().changed, 1);
        assert_eq!(text_search(&dir, "zanzibar", 10).unwrap().len(), 1);
        assert_eq!(refresh().changed, 0);
        use std::io::Write;
        let appended = serde_json::json!({"type":"user","uuid":"last","sessionId":"adopted",
            "message":{"role":"user","content":"appendmarker"}});
        writeln!(std::fs::OpenOptions::new().append(true).open(&p).unwrap(), "{appended}").unwrap();
        let appended = refresh();
        assert_eq!((appended.changed, appended.resumed), (1, 1));
        std::fs::write(&file, "kilimanj").unwrap(); // Same length, prefix output was reused above.
        let base = cv_core::source::Probe::default().snapshot(&r);
        assert!(cv_core::catalog::AffordanceTable::load()
            .counts_with_source(&r, &base)
            .is_some());
        assert!(!cv_core::events::SyncTable::load().needs_ingest_with_source(&r, &base));
        let changed = refresh();
        assert_eq!((changed.changed, changed.resumed), (1, 0));
        assert!(text_search(&dir, "zanzibar", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "kilimanj", 10).unwrap().len(), 1);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(refresh().changed, 1);
        assert!(text_search(&dir, "kilimanj", 10).unwrap().is_empty());
        assert_eq!(refresh().changed, 0);
        std::fs::write(&file, "zanzibar").unwrap(); // The resumed read set retains absence too.
        assert_eq!(refresh().changed, 1);
        assert_eq!(text_search(&dir, "zanzibar", 10).unwrap().len(), 1);
        assert_eq!(refresh().changed, 0);
        for d in [dir, sdir, external] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn directory_wire_sweep_resumes_and_keeps_events_counts_and_search_exact() {
        use std::io::Write as _;
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let wire = sdir.join("agents/main/wire.jsonl");
        std::fs::create_dir_all(wire.parent().unwrap()).unwrap();
        let controls = sdir.join("state.json");
        std::fs::write(&controls, "{\"cwd\":\"/before\"}").unwrap();
        let line = format!(
            "{}\n",
            serde_json::json!({"type":"context.append_message",
            "message":{"role":"user","content":"zanzibar"}})
        );
        std::fs::write(&wire, line.repeat(8192)).unwrap();
        let r = cv_core::SessionRef::new("directory", cv_core::Harness::KimiCode, &sdir);
        let refresh = |rebuild| sweep(&dir, vec![r.clone()], rebuild, false).unwrap();
        assert_eq!(refresh(false).changed, 1);
        assert_eq!(refresh(false).changed, 0);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap()
            .write_all(format!("{}{}\n{}\n{}\n", line.repeat(4),
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s"}}),
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"t","stepUuid":"s","toolCallId":"t","name":"Read","args":{"path":"/before/a.rs"}}}),
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s"}})).as_bytes())
            .unwrap();
        let appended = refresh(false);
        assert_eq!((appended.changed, appended.resumed), (1, 1));
        let source = cv_core::source::Probe::default().snapshot(&r);
        assert!(!cv_core::events::needs_ingest(
            &r,
            cv_core::events::file_mtime_ns(&r.path)
        ));
        let counts = cv_core::catalog::AffordanceTable::load()
            .counts_with_source(&r, &source)
            .unwrap();
        let before = observe(&dir, std::slice::from_ref(&r), &["zanzibar".into()]);
        let rows = cv_core::events::events_for("kimi-code", &r.id, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "file_read");
        let events = format!("{rows:?}");
        assert_eq!(refresh(true).resumed, 0);
        assert_eq!(counts, cv_core::catalog::AffordanceTable::load().counts(&r).unwrap());
        assert_eq!(before, observe(&dir, std::slice::from_ref(&r), &["zanzibar".into()]));
        assert_eq!(
            events,
            format!("{:?}", cv_core::events::events_for("kimi-code", &r.id, None))
        );
        std::fs::write(&controls, "{\"cwd\":\"/changed\"}").unwrap();
        let changed = refresh(false);
        assert_eq!((changed.changed, changed.resumed), (1, 0));
        assert_eq!(refresh(false).changed, 0);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(sdir).unwrap();
    }

    #[test]
    fn conditional_fork_read_sets_track_new_and_rewritten_siblings() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let p = sdir.join("fork.jsonl");
        let copied = serde_json::json!({"type":"user","uuid":"shared","sessionId":"fork",
            "timestamp":"2026-09-19T00:00:00Z","message":{"role":"user","content":"forkbody"}});
        let queued = serde_json::json!({"type":"queue-operation","timestamp":"2026-09-19T01:00:00Z"});
        std::fs::write(&p, format!("{queued}\n{copied}\n")).unwrap();
        let r = sref("fork", "fork", p.display().to_string());
        let refresh = || sweep(&dir, vec![r.clone()], false, false).unwrap();
        let counts = || cv_core::catalog::AffordanceTable::load().counts(&r).unwrap();
        assert_eq!(refresh().changed, 1);
        assert_eq!(counts().get(&cv_core::affordance::Kind::Fork).copied().unwrap_or(0), 0);
        assert_eq!(refresh().changed, 0);
        let sibling = sdir.join("source.jsonl");
        std::fs::write(&sibling, format!("{copied}\n")).unwrap();
        assert_eq!(refresh().changed, 1);
        assert_eq!(counts().get(&cv_core::affordance::Kind::Fork), Some(&1));
        assert_eq!(refresh().changed, 0);
        let directory_mtime = std::fs::metadata(&sdir).unwrap().modified().unwrap();
        std::fs::write(&sibling, format!("{copied}\n").replace("shared", "other!")).unwrap();
        assert_eq!(std::fs::metadata(&sdir).unwrap().modified().unwrap(), directory_mtime);
        assert_eq!(refresh().changed, 1);
        assert_eq!(counts().get(&cv_core::affordance::Kind::Fork).copied().unwrap_or(0), 0);
        assert_eq!(refresh().changed, 0);
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn guarded_empty_kimi_sweep_skips_without_parse_or_writer_work() {
        let _home = IsolatedHome::new();
        let (dir, base) = (tmpdir(), tmpdir());
        let session = base.join("empty-kimi");
        std::fs::create_dir(&session).unwrap();
        let path = session.join("context.jsonl");
        std::fs::write(&path, []).unwrap();
        let r = cv_core::SessionRef::new("empty-kimi", cv_core::Harness::Kimi, &session);
        let cold = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((cold.changed, cold.errors), (1, 0));
        let before = observe(&dir, std::slice::from_ref(&r), &[]);
        let warm = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((warm.changed, warm.errors, warm.work), (0, 0, SweepWork::default()));
        assert_eq!(before, observe(&dir, std::slice::from_ref(&r), &[]));
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 1);
        std::fs::write(&path, []).unwrap();
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 1);
        #[cfg(unix)]
        {
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let replacement = session.join("replacement");
            std::fs::write(&replacement, []).unwrap();
            std::fs::File::open(&replacement)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(mtime))
                .unwrap();
            std::fs::rename(&replacement, &path).unwrap();
            assert_eq!(
                sweep(&dir, vec![r.clone()], false, false).unwrap().changed,
                1,
                "same-tuple replacement misses identity guard"
            );
        }
        let warm = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((warm.changed, warm.work), (0, SweepWork::default()));
        assert_eq!(
            observe(&dir, std::slice::from_ref(&r), &[]),
            whole_sweep(std::slice::from_ref(&r), &[])
        );
        for path in [dir, base] {
            std::fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn incremental_skips_unchanged_and_reaps_missing() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "x", "alpha beta gamma");
        // Seed the index with one session.
        index_refs(&dir, &[sref("x", "first", p.clone())], false).unwrap();
        assert_eq!(text_search(&dir, "alpha", 10).unwrap().len(), 1);

        // A direct full-rebuild replaces contents (old doc gone).
        let p2 = write_claude(&sdir, "y", "delta epsilon");
        index_refs(&dir, &[sref("y", "second", p2)], false).unwrap();
        assert!(text_search(&dir, "alpha", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "delta", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Rewrite a file with its own byte-identical content — bumping mtime (the OS stamps "now" on
    /// write) while leaving size unchanged. Set a distinct time explicitly so this works on
    /// filesystems whose write timestamps have coarse resolution.
    fn bump_mtime(path: &str) {
        let content = std::fs::read(path).unwrap();
        let before = std::fs::metadata(path).unwrap().len();
        let next_mtime = std::fs::metadata(path).unwrap().modified().unwrap() + std::time::Duration::from_secs(1);
        std::fs::write(path, &content).unwrap();
        std::fs::File::open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(next_mtime))
            .unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            before,
            "bump_mtime must not change size"
        );
    }

    /// Append a line to a session transcript (size grows) — a real content change.
    fn append(path: &str, extra: &str) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "type": "user",
            "uuid": "append",
            "message": { "role": "user", "content": extra }
        });
        let before = std::fs::metadata(path).unwrap().len();
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
        assert!(
            std::fs::metadata(path).unwrap().len() > before,
            "append must grow the file"
        );
    }

    /// Equal length does not establish equal content. Same-size edits update search results;
    /// timestamp changes conservatively re-index, while unchanged sources skip.
    #[test]
    fn incremental_detects_same_size_rewrites_and_mtime_changes() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let p1 = write_claude(&sdir, "s1", "alpha session one body");
        let p2 = write_claude(&sdir, "s2", "beta session two body");
        let refs = || vec![sref("s1", "one", p1.clone()), sref("s2", "two", p2.clone())];

        // Initial index: both sessions are new → both indexed.
        assert_eq!(
            index_refs_incremental(&dir, &refs()).unwrap(),
            2,
            "first pass indexes both new sessions"
        );

        assert_eq!(
            index_refs_incremental(&dir, &refs()).unwrap(),
            0,
            "unchanged sources skip"
        );

        let original = std::fs::read_to_string(&p1).unwrap();
        let edited = original.replace("alpha", "omega");
        assert_eq!(edited.len(), original.len());
        std::fs::write(&p1, edited).unwrap();
        bump_mtime(&p1);
        assert_eq!(
            index_refs_incremental(&dir, &refs()).unwrap(),
            1,
            "same-size rewrite re-indexes"
        );
        assert!(text_search(&dir, "alpha", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "omega", 10).unwrap().len(), 1);

        // Byte-identical rewrites still require a pass: file metadata cannot establish equality.
        bump_mtime(&p1);
        bump_mtime(&p2);
        let reindexed = index_refs_incremental(&dir, &refs()).unwrap();
        assert_eq!(reindexed, 2, "mtime changes conservatively re-index both sources");

        // Append to exactly one session (size grows) → that one re-indexes, the other still skips.
        append(&p2, "gamma appended tail");
        let reindexed = index_refs_incremental(&dir, &refs()).unwrap();
        assert_eq!(
            reindexed, 1,
            "a real append (size grew) must re-index exactly the appended session — got {reindexed}"
        );
        // And the appended content is now searchable.
        let hits = text_search(&dir, "gamma", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s2");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn copied_session_sweep_matches_resolver_authority_and_clean_oracle() {
        let home = IsolatedHome::new();
        let dir = tmpdir();
        let oracle = tmpdir();
        let older_path = write_claude_msgs(&home.home, "old-copy", &[("user", "sapphire old body")]);
        let newer_path = write_claude_msgs(&home.home, "new-copy", &[("user", "amber newest body")]);
        let mut older = sref("shared", "old title", older_path);
        let mut newer = sref("shared", "new title", newer_path);
        older.updated_at = chrono::DateTime::from_timestamp(1, 0);
        newer.updated_at = chrono::DateTime::from_timestamp(2, 0);
        let stored = |path: &Path| {
            use tantivy::schema::document::Document;
            let index = Index::open_in_dir(path).unwrap();
            let searcher = index.reader().unwrap().searcher();
            let mut docs = Vec::new();
            for segment in searcher.segment_readers() {
                let reader = segment.get_store_reader(0).unwrap();
                for id in segment.doc_ids_alive() {
                    let doc: TantivyDocument = reader.get(id).unwrap();
                    docs.push(doc.to_json(&index.schema()));
                }
            }
            docs.sort();
            docs
        };
        let first = sweep(&dir, vec![older.clone(), newer.clone()], true, false).unwrap();
        assert_eq!((first.total, first.copies, first.changed), (2, 1, 1));
        index_refs(&oracle, std::slice::from_ref(&newer), false).unwrap();
        assert_eq!(stored(&dir), stored(&oracle));
        assert_eq!(
            text_search_ids(&dir, "amber").unwrap(),
            HashSet::from(["shared".to_owned()])
        );
        assert!(text_search_ids(&dir, "sapphire").unwrap().is_empty());
        let before = stored(&dir);
        let unchanged = sweep(&dir, vec![newer.clone(), older.clone()], false, false).unwrap();
        assert_eq!((unchanged.changed, unchanged.copies), (0, 1));
        assert_eq!(stored(&dir), before);
        // Timestamp ties use the same deterministic path order independent of input order.
        older.updated_at = newer.updated_at;
        let preferred = if older.preferred_copy_cmp(&newer).is_lt() {
            older.clone()
        } else {
            newer.clone()
        };
        for refs in [vec![older.clone(), newer.clone()], vec![newer.clone(), older.clone()]] {
            assert_eq!(preferred_index_copies(refs)[0].path, preferred.path);
        }
        // Repair a current-schema index with duplicate meta documents even if both carry
        // exactly the preferred metadata and guards, then skip its next unchanged pass.
        index_refs(&dir, &[newer.clone(), newer.clone()], true).unwrap();
        assert_eq!(stored(&dir).len(), 4);
        assert_eq!(
            sweep(&dir, vec![older.clone(), newer.clone()], false, false)
                .unwrap()
                .changed,
            1
        );
        assert_eq!(stored(&dir), stored(&oracle));
        assert_eq!(
            sweep(&dir, vec![newer.clone(), older.clone()], false, false)
                .unwrap()
                .changed,
            0
        );
        // A stale supplied ref cannot hide a still-live copy; no live copy must still error.
        std::fs::remove_file(&newer.path).unwrap();
        let fallback = sweep(&dir, vec![newer.clone(), older.clone()], false, false).unwrap();
        assert_eq!(fallback.changed, 1);
        assert_eq!(
            text_search_ids(&dir, "sapphire").unwrap(),
            HashSet::from(["shared".to_owned()])
        );
        std::fs::remove_file(&older.path).unwrap();
        assert_eq!(sweep(&dir, vec![older, newer], false, false).unwrap().errors, 1);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(oracle).unwrap();
    }

    #[test]
    fn duplicate_meta_repair_with_valid_append_mark_replaces_the_whole_prefix() {
        let home = IsolatedHome::new();
        let dir = tmpdir();
        let oracle = tmpdir();
        let body = "sapphire ".repeat(8192);
        let turns: Vec<_> = (0..70).map(|_| ("user", body.as_str())).collect();
        let path = write_claude_msgs(&home.home, "repair-append", &turns);
        let r = sref("repair-append", "same metadata", path);
        sweep(&dir, vec![r.clone()], true, false).unwrap();
        assert!(
            cv_core::resume::MarkTable::load().get(&r).is_some(),
            "fixture must establish a production checkpoint"
        );
        // The independent old collector creates duplicate prefix/meta records, while leaving
        // the valid production checkpoint available for the subsequent real append.
        index_refs(&dir, &[r.clone(), r.clone()], true).unwrap();
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            "{}",
            serde_json::json!({"type":"user","uuid":"tail","sessionId":"repair-append",
                "message":{"role":"user","content":"amber appended tail"}})
        )
        .unwrap();
        let marks = cv_core::resume::MarkTable::load();
        let head = marks.get(&r).unwrap();
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let version = cv_core::resume::version(adapter.as_ref(), RESUME_TAG).unwrap();
        assert!(head.holds(&r.path, std::fs::metadata(&r.path).unwrap().len(), &version));
        assert!(cv_core::resume::state(&r, head)
            .unwrap()
            .get::<FtsState>(FTS_LAYER)
            .is_some());
        let (index, fields) = open_existing(&dir).unwrap();
        let plan = SweepPlanner {
            index: &index,
            f: &fields,
            dir: &dir,
            refs: std::slice::from_ref(&r),
            rebuild: false,
            subagents: false,
        }
        .plan()
        .unwrap();
        assert!(plan.jobs[0].existing && plan.jobs[0].dependencies_fresh);
        assert!(
            !plan.jobs[0].grew,
            "corruption must disable otherwise valid append reuse"
        );
        let repaired = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((repaired.changed, repaired.resumed, repaired.errors), (1, 0, 0));
        index_refs(&oracle, std::slice::from_ref(&r), false).unwrap();
        let stored = |path: &Path| {
            use tantivy::schema::document::Document;
            let index = Index::open_in_dir(path).unwrap();
            let searcher = index.reader().unwrap().searcher();
            let mut docs = Vec::new();
            for segment in searcher.segment_readers() {
                let store = segment.get_store_reader(0).unwrap();
                for id in segment.doc_ids_alive() {
                    let doc: TantivyDocument = store.get(id).unwrap();
                    docs.push(doc.to_json(&index.schema()));
                }
            }
            docs.sort();
            docs
        };
        assert_eq!(stored(&dir), stored(&oracle));
        assert_eq!(
            text_search_ids(&dir, "amber").unwrap(),
            text_search_ids(&oracle, "amber").unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(oracle).unwrap();
    }

    #[test]
    fn incompatible_event_copy_authority_forces_full_append_replay() {
        let home = IsolatedHome::new();
        let dir = tmpdir();
        let oracle = tmpdir();
        let path = home.home.join("canonical.jsonl");
        let edit = |id: &str, target: &str| {
            serde_json::json!({"type":"assistant","uuid":id,
            "sessionId":"copy-events","message":{"role":"assistant","content":[{"type":"tool_use",
                "id":id,"name":"Edit","input":{"file_path":target,"old_string":"a","new_string":"b"}}]}})
            .to_string()
        };
        let mut bytes = edit("prefix", "/copy-authority/prefix.rs") + "\n";
        for n in 0..8 {
            bytes.push_str(
                &serde_json::json!({"type":"user","uuid":format!("u{n}"),"sessionId":"copy-events",
                "message":{"role":"user","content":"sapphire ".repeat(1024)}})
                .to_string(),
            );
            bytes.push('\n');
        }
        std::fs::write(&path, bytes).unwrap();
        let r = sref("copy-events", "same title", path.to_string_lossy().into_owned());
        sweep(&dir, vec![r.clone()], true, false).unwrap();
        let other_path = home.home.join("foreign.jsonl");
        std::fs::write(&other_path, edit("foreign", "/copy-authority/foreign.rs") + "\n").unwrap();
        let other = cv_core::SessionRef {
            path: other_path,
            ..r.clone()
        };
        cv_core::events::ingest_ref(&other).unwrap();
        assert!(!cv_core::events::SyncTable::load().can_resume(&r, &Default::default()));
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            "{}",
            edit("tail", "/copy-authority/tail.rs")
        )
        .unwrap();
        let marks = cv_core::resume::MarkTable::load();
        let head = marks.get(&r).unwrap();
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let version = cv_core::resume::version(adapter.as_ref(), RESUME_TAG).unwrap();
        assert!(head.holds(&r.path, std::fs::metadata(&r.path).unwrap().len(), &version));
        let replay = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((replay.changed, replay.resumed, replay.errors), (1, 0, 0));
        let events = || {
            cv_core::events::events_for("claude", "copy-events", None)
                .into_iter()
                .map(|e| {
                    (
                        e.msg_idx,
                        e.kind,
                        e.tool,
                        e.target,
                        e.detail,
                        e.parent_id,
                        e.agent_id,
                        e.workflow,
                    )
                })
                .collect::<Vec<_>>()
        };
        let actual = events();
        assert_eq!(actual.len(), 2);
        assert!(actual
            .iter()
            .all(|e| e.3.as_deref() != Some("/copy-authority/foreign.rs")));
        index_refs(&oracle, std::slice::from_ref(&r), true).unwrap();
        assert_eq!(
            actual,
            events(),
            "whole replay must replace foreign events and restore canonical prefix"
        );
        assert_eq!(
            text_search_ids(&dir, "sapphire").unwrap(),
            text_search_ids(&oracle, "sapphire").unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(oracle).unwrap();
    }

    /// Every source requires both mtime and size to match. Unknown or empty sources are stale.
    #[test]
    fn fts_is_fresh_requires_mtime_and_size_for_every_source() {
        let append_only = sref("append", "append", "/tmp/append.jsonl".into());
        let non_append = cv_core::SessionRef {
            id: "cline-task".into(),
            harness: cv_core::Harness::Cline,
            path: "/tmp/task/api_conversation_history.json".into(),
            cwd: Some("/home/u/proj".into()),
            title: Some("task".into()),
            created_at: None,
            updated_at: None,
            message_count: 1,
        };

        assert!(fts_is_fresh(&append_only, Some(&(111, 4096)), 111, 4096, || true));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 4096)), 222, 4096, || true));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 4096)), 222, 4096, || panic!(
            "tuple miss must not check primary identity"
        )));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 4096)), 111, 8192, || true));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 0)), 111, 0, || false));
        assert!(fts_is_fresh(&append_only, Some(&(111, 0)), 111, 0, || true));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 4096)), 111, 4096, || false));
        assert!(!fts_is_fresh(&append_only, None, 111, 4096, || true));

        assert!(fts_is_fresh(&non_append, Some(&(111, 4096)), 111, 4096, || true));
        assert!(!fts_is_fresh(&non_append, Some(&(111, 4096)), 222, 4096, || true));
        assert!(!fts_is_fresh(&non_append, Some(&(111, 4096)), 111, 8192, || true));
    }

    #[test]
    fn auxiliary_directory_changes_refresh_search_events_and_counts() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        std::fs::write(
            sdir.join("summary.json"),
            r#"{"info":{"id":"grok-child","cwd":"/repo"}}"#,
        )
        .unwrap();
        let transcript = sdir.join("chat_history.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\",\"content\":\"zqbeforechild\"}\n").unwrap();
        let r = cv_core::SessionRef {
            harness: cv_core::Harness::Grok,
            path: sdir.clone(),
            ..sref("grok-child", "child", "unused".into())
        };
        sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 0);
        let primary = cv_core::offsets::file_sig(&sdir);
        let line = serde_json::json!({"type":"assistant","content":"zqafterchild",
            "tool_calls":[{"id":"call","name":"read_file","arguments":"{\"path\":\"/repo/new.rs\"}"}]});
        use std::io::Write as _;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(),
            "{line}"
        )
        .unwrap();
        assert_eq!(
            cv_core::offsets::file_sig(&sdir),
            primary,
            "child append leaves directory signature unchanged"
        );
        assert!(cv_core::events::needs_ingest(&r, primary.0));
        assert!(cv_core::catalog::AffordanceTable::load().stale(&r));
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 1);
        assert_eq!(text_search(&dir, "zqafterchild", 10).unwrap().len(), 1);
        assert_eq!(cv_core::events::events_for("grok", &r.id, Some("file_read")).len(), 1);
        assert!(!cv_core::events::needs_ingest(&r, primary.0));
        assert!(!cv_core::catalog::AffordanceTable::load().stale(&r));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn auxiliary_claude_agent_metadata_invalidates_each_consumer() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let parent = write_claude(&sdir, "parent-meta", "orchestrator");
        let agent = write_subagent(&parent, "meta", "zqmetabody");
        let r = sref("agent-meta", "agent", agent.to_string_lossy().into_owned());
        sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 0);
        let primary = cv_core::offsets::file_sig(&agent);
        std::fs::write(
            agent.with_extension("meta.json"),
            r#"{"agentType":"reviewer","description":"new role","toolUseId":"spawn"}"#,
        )
        .unwrap();
        assert_eq!(cv_core::offsets::file_sig(&agent), primary);
        assert!(cv_core::events::needs_ingest(&r, primary.0));
        assert!(cv_core::catalog::AffordanceTable::load().stale(&r));
        cv_core::events::ingest_ref(&r).unwrap();
        assert!(!cv_core::events::needs_ingest(&r, primary.0));
        assert!(
            cv_core::catalog::AffordanceTable::load().stale(&r),
            "events refresh cannot refresh counts"
        );
        assert_eq!(
            sweep(&dir, vec![r.clone()], false, false).unwrap().changed,
            1,
            "events refresh cannot refresh FTS"
        );
        assert!(!cv_core::catalog::AffordanceTable::load().stale(&r));
        assert_eq!(sweep(&dir, vec![r], false, false).unwrap().changed, 0);
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn auxiliary_wal_only_updates_refresh_search_events_and_counts() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let db = sdir.join("sessions.db");
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
            CREATE TABLE sessions(id TEXT PRIMARY KEY,source TEXT NOT NULL,model TEXT,started_at REAL NOT NULL,ended_at REAL,message_count INTEGER DEFAULT 0,title TEXT);
            CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL,role TEXT NOT NULL,content TEXT,tool_call_id TEXT,tool_calls TEXT,tool_name TEXT,timestamp REAL NOT NULL);
            INSERT INTO sessions(id,source,started_at) VALUES('wal-session','cli',1000);
            INSERT INTO messages(session_id,role,content,timestamp) VALUES('wal-session','user','zqbeforewal',1001);").unwrap();
        let r = cv_core::SessionRef {
            harness: cv_core::Harness::Hermes,
            path: db.clone(),
            ..sref("wal-session", "wal", "unused".into())
        };
        sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 0);
        let primary = cv_core::offsets::file_sig(&db);
        writer.execute_batch("INSERT INTO messages(session_id,role,content,timestamp) VALUES('wal-session','user','zqafterwal',1002);
            INSERT INTO messages(session_id,role,tool_calls,timestamp) VALUES('wal-session','assistant','[{\"id\":\"c\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"/repo/new.rs\\\"}\"}}]',1003);").unwrap();
        assert_eq!(cv_core::offsets::file_sig(&db), primary, "write stayed in WAL");
        assert!(cv_core::events::needs_ingest(&r, primary.0));
        assert!(cv_core::catalog::AffordanceTable::load().stale(&r));
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 1);
        assert_eq!(text_search(&dir, "zqafterwal", 10).unwrap().len(), 1);
        assert_eq!(cv_core::events::events_for("hermes", &r.id, Some("file_read")).len(), 1);
        assert!(!cv_core::events::needs_ingest(&r, primary.0));
        assert!(!cv_core::catalog::AffordanceTable::load().stale(&r));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// The core invariant of `ChunkSink`: a body larger than [`CHUNK_BYTES`] is flushed into several
    /// tantivy docs that all share the session id. Search must (a) still find a term living only in a
    /// late chunk, and (b) dedup a term present in *every* chunk back to a single row per id.
    #[test]
    fn oversized_body_is_chunked_yet_dedups_to_one_hit() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // ~5.6 MB of a repeated common token (lands in every chunk) with a unique marker at the very
        // end (lands only in the final chunk) — forcing ≥2 docs for the one session.
        let mut body = "padding ".repeat(700_000);
        assert!(body.len() > CHUNK_BYTES, "fixture must exceed one chunk");
        body.push_str("zqxmarker");
        let p = write_claude(&sdir, "big", &body);
        index_refs(&dir, &[sref("big", "huge session", p)], false).unwrap();

        // Term in every chunk → multiple matching docs, but deduped to exactly one row for the id.
        let hits = text_search(&dir, "padding", 10).unwrap();
        assert_eq!(hits.len(), 1, "chunks of one session must dedup to a single hit");
        assert_eq!(hits[0].id, "big");

        // Term only in the trailing chunk → still indexed and findable (proves later chunks are added).
        let hits = text_search(&dir, "zqxmarker", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "big");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// A session with no discovery title labels itself from the first user message, and the body
    /// accumulates across roles — so an assistant-only term is searchable too.
    #[test]
    fn title_falls_back_to_first_user_and_body_spans_roles() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude_msgs(
            &sdir,
            "m",
            &[
                ("user", "alpha question about tantivy"),
                ("assistant", "beta answer mentioning bm25"),
            ],
        );
        index_refs(&dir, &[sref_untitled("m", p)], false).unwrap();

        // Assistant-turn term is in the body (roles accumulate into one searchable blob).
        let hits = text_search(&dir, "bm25", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m");
        // No ai-title → label is the first user message's text.
        assert_eq!(hits[0].title.as_deref(), Some("alpha question about tantivy"));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// `live_snippet` must stay O(cap) on a session fronted by one giant field: a term within the
    /// scan cap windows normally, a term hiding *beyond* the cap inside the same giant field falls
    /// back to the head instead of materializing the whole record to find it.
    #[test]
    fn live_snippet_caps_giant_fields() {
        let sdir = tmpdir();
        let mut body = String::from("zqearly marker then padding ");
        body.push_str(&"pad ".repeat(2 * SNIPPET_SCAN_CAP / 4)); // 2× the cap of filler
        body.push_str(" zqlate");
        assert!(body.len() > SNIPPET_SCAN_CAP);
        let p = write_claude(&sdir, "giant", &body);

        let snip = live_snippet(&p, "claude", "zqearly").expect("snippet");
        assert!(snip.contains("zqearly"), "in-cap term should window: {snip:?}");

        let snip = live_snippet(&p, "claude", "zqlate").expect("snippet");
        assert!(
            !snip.contains("zqlate") && snip.starts_with("zqearly"),
            "beyond-cap term must fall back to the head, got: {snip:?}"
        );

        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn indexed_snippet_maps_casefold_offsets_to_original_unicode() {
        let body = format!("{}target {}", "İ".repeat(200), "tail ".repeat(100));
        let pos = body.find("target").unwrap();
        assert_eq!(make_snippet(&body, "target"), window(&body, pos, 6));
        assert!(make_snippet(&body, "target").contains("target"));
        for (body, query, original) in [
            ("İstanbul", "i", "İ"),
            ("ΟΣ next", "ος", "ΟΣ"),
            ("ΟΣΑ next", "οσα", "ΟΣΑ"),
        ] {
            assert_eq!(make_snippet(body, query), window(body, 0, original.len()));
        }
    }

    #[test]
    fn unreadable_index_metadata_does_not_become_an_empty_refresh_plan() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(f.id => "known", f.kind => KIND_META, f.mtime => 1i64, f.size => 2i64))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        let store = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "store"))
            .unwrap();
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        std::fs::write(store, b"invalid stored field data").unwrap();
        assert!(read_indexed_metadata(&index, &f, true).is_err());
        assert!(sweep(&dir, Vec::new(), false, false).is_err());
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_index_stamps_preserve_identity_for_replacement() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(f.id => "repair", f.kind => KIND_META,
            f.title => "stale needle", f.preview => "stale preview"))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        let (sigs, _, metadata, _) = read_indexed_metadata(&index, &f, true).unwrap();
        assert_eq!(sigs.get("repair"), Some(&(i64::MIN, i64::MIN)));
        assert!(!metadata.contains_key("repair"));
        assert_eq!(newest_indexed_mtime(&dir), None);
        let input = dir.join("repair.jsonl");
        std::fs::write(&input, "{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"repair\",\"message\":{\"role\":\"user\",\"content\":\"current needle\"}}\n").unwrap();
        let reference = cv_core::SessionRef::new("repair", cv_core::Harness::Claude, &input);
        let result = sweep(&dir, vec![reference], false, false).unwrap();
        assert_eq!((result.changed, result.errors), (1, 0));
        let (index, f) = open_existing(&dir).unwrap();
        assert!(read_indexed_metadata(&index, &f, true).unwrap().3.is_empty());
        let hit = text_search_indexed(&dir, "needle", 1).unwrap().remove(0);
        assert_eq!(hit.title.as_deref(), Some("current needle"));
        // Anonymous metadata cannot be repaired by deleting a session-id term.
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(f.kind => KIND_META, f.mtime => 1i64, f.size => 2i64))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        assert!(read_indexed_metadata(&index, &f, true).is_err());
        let before = std::fs::read(dir.join("meta.json")).unwrap();
        assert!(sweep(&dir, Vec::new(), false, false).is_err());
        assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn make_snippet_windows_match_then_falls_back_to_head() {
        let body = "the quick brown fox jumps over the lazy dog";
        let snip = make_snippet(body, "brown");
        assert!(snip.contains("brown"), "snippet should window the match: {snip:?}");
        // No term matches → fall back to the head of the body.
        assert_eq!(make_snippet("hello world", "kubernetes"), "hello world");
    }

    #[test]
    fn query_terms_drops_fields_and_booleans() {
        let terms = query_terms("harness:claude Tantivy AND bm25:score NOT pandas");
        assert_eq!(terms, vec!["tantivy", "pandas"]);
    }

    /// A query made *entirely* of fielded/boolean tokens has no bare terms to window on — the
    /// snippet must silently fall back to the head of the body, not panic or come back empty.
    #[test]
    fn all_fielded_query_snippets_fall_back_to_head() {
        let _home = IsolatedHome::new();
        assert!(query_terms("harness:claude AND cwd:proj").is_empty());
        assert_eq!(make_snippet("hello world", "harness:claude"), "hello world");
        assert_eq!(
            make_snippet("hello world", "harness:claude AND cwd:proj NOT id:x"),
            "hello world"
        );
        // End-to-end: a purely-fielded query still produces a non-empty (head) snippet on a hit.
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "ff", "fielded only query body text");
        index_refs(&dir, &[sref("ff", "fielded", p)], false).unwrap();
        let hits = text_search(&dir, "harness:claude", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("fielded only query body"),
            "head fallback snippet expected: {:?}",
            hits[0].snippet
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Fix for the blank-snippet hole: when a hit's source file has been deleted since indexing,
    /// the snippet falls back to the head-of-body preview stored on the first chunk doc, with a
    /// marker explaining why there's no live context.
    #[test]
    fn missing_source_falls_back_to_stored_preview_with_marker() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let body = "We built a full text\nsearch engine   with tantivy and BM25 scoring.";
        let p = write_claude(&sdir, "gone", body);
        index_refs(&dir, &[sref("gone", "doomed session", p.clone())], false).unwrap();

        // While the file is present, the snippet is live (no marker).
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.contains("(source file moved)"));

        std::fs::remove_file(&p).unwrap();

        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "hit should survive the file vanishing");
        // Preview is whitespace-collapsed: the newline and space-run become single spaces.
        assert!(
            hits[0]
                .snippet
                .contains("We built a full text search engine with tantivy"),
            "stored preview expected: {:?}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.ends_with("(source file moved)"),
            "marker expected: {:?}",
            hits[0].snippet
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// The event ride-along, end to end through the production `index_session` tee: one indexing
    /// pass writes BOTH searchable tantivy docs and queryable event rows in the catalog db
    /// (isolated via `CLUSTERVISION_HOME`).
    #[test]
    fn indexing_tees_events_into_the_catalog() {
        let h = IsolatedHome::new();
        let dir = tmpdir();

        // A claude transcript whose assistant turn edits a file and runs a command.
        let p = h.home.join("tee-evt.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user", "uuid": "u0", "sessionId": "tee-evt",
                "message": {"role": "user", "content": "rework the indexer"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "tee-evt",
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "on it — touching the indexer now"},
                    {"type": "tool_use", "id": "t1", "name": "Edit",
                     "input": {"file_path": "crates/cv-search/src/fts.rs",
                               "old_string": "a", "new_string": "b"}},
                    {"type": "tool_use", "id": "t2", "name": "Bash",
                     "input": {"command": "cargo test -p cv-search"}}
                ]}
            }),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        let mut r = sref("tee-evt", "indexer rework", p.display().to_string());
        r.cwd = Some("/repo".into());

        index_refs(&dir, &[r], true).unwrap();

        // The text landed in tantivy…
        let hits = text_search(&dir, "indexer", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "tee-evt");

        // …and the SAME pass landed events in the catalog.
        let rows = cv_core::events::events_for("claude", "tee-evt", None);
        let edit = rows.iter().find(|e| e.kind == "file_edit").expect("edit event");
        assert_eq!(edit.target.as_deref(), Some("/repo/crates/cv-search/src/fts.rs"));
        let cmd = rows.iter().find(|e| e.kind == "command").expect("command event");
        assert_eq!(cmd.target.as_deref(), Some("cargo test -p cv-search"));

        // The touched query resolves it by path suffix.
        let touched = cv_core::events::sessions_touching("crates/cv-search/src/fts.rs", true);
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].session_id, "tee-evt");
        assert_eq!(touched[0].edits, 1);

        std::fs::remove_dir_all(&dir).ok();
        // `h` drops here: removes CLUSTERVISION_HOME, deletes the temp home, releases the lock.
    }

    /// Write a sub-agent transcript in the on-disk layout `subagent_tree_of` expects:
    /// `<parent_dir>/<parent_stem>/subagents/agent-<agent_id>.jsonl`. `body` is one user message.
    /// Returns the agent transcript path (so a test can prove it exists / mtime it).
    fn write_subagent(parent_path: &str, agent_id: &str, body: &str) -> std::path::PathBuf {
        let pp = Path::new(parent_path);
        let stem = pp.file_stem().unwrap().to_str().unwrap();
        let dir = pp.parent().unwrap().join(stem).join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("agent-{agent_id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": format!("agent-{agent_id}"),
            "sessionId": format!("agent-{agent_id}"),
            "message": { "role": "user", "content": body }
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p
    }

    #[test]
    fn refresh_unchanged_counts_requested_fresh_sessions_and_folded_lanes() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let pp = write_claude(&sdir, "count-parent", "parent body");
        write_subagent(&pp, "count-child", "child body");
        let parent = sref("count-parent", "parent session", pp.clone());
        let first = sweep(&dir, vec![parent.clone()], false, true).unwrap();
        assert_eq!((first.changed, first.unchanged), (2, 0));
        let fresh = sweep(&dir, vec![parent.clone()], false, true).unwrap();
        assert_eq!((fresh.changed, fresh.unchanged), (0, 2));
        // A plain refresh preserves the forest but counts only its requested top-level scope.
        let plain = sweep(&dir, vec![parent.clone()], false, false).unwrap();
        assert_eq!((plain.changed, plain.unchanged), (0, 1));
        write_subagent(&pp, "count-child", "a substantially longer changed child body");
        let changed = sweep(&dir, vec![parent], false, true).unwrap();
        assert_eq!((changed.changed, changed.unchanged), (1, 1));
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(sdir).ok();
    }

    /// `cv index --subagents` end to end: a string living ONLY inside a sub-agent transcript is
    /// (a) NOT findable when the forest is not folded, and (b) findable AND provenance-tagged
    /// (parent id + agent id) once it is. Exercises the production forest path
    /// (`index_one_subagent`), not a parallel one.
    #[test]
    fn subagent_forest_is_searchable_and_provenance_tagged_only_when_folded() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // Parent transcript + a sub-agent whose body alone holds the unique marker.
        let pp = write_claude(&sdir, "parent1", "orchestrator planning the work");
        write_subagent(&pp, "child9", "deep in the weeds: zqsubmarker lives only here");
        let parent = sref("parent1", "parent session", pp);

        // (a) Top-level only: the sub-agent marker is invisible.
        index_refs(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert!(
            text_search(&dir, "zqsubmarker", 10).unwrap().is_empty(),
            "without --subagents a sub-agent-only term must NOT be findable"
        );
        // The parent itself is still indexed.
        assert_eq!(text_search(&dir, "orchestrator", 10).unwrap().len(), 1);
        // The empty-result nudge fires: a top-level-only index carries no folded forest.
        assert!(
            !has_subagent_docs(&dir),
            "a top-level-only index must report no sub-agent forest (so `cv search` nudges --subagents)"
        );

        // (b) Fold the forest: the marker becomes findable, tagged back to its parent + agent.
        let folded = index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert_eq!(folded, 1, "exactly one sub-agent transcript folded in");
        assert!(
            has_subagent_docs(&dir),
            "once folded, the index must report a sub-agent forest (so the nudge goes quiet)"
        );
        let hits = text_search(&dir, "zqsubmarker", 10).unwrap();
        assert_eq!(hits.len(), 1, "sub-agent term findable once folded");
        let h = &hits[0];
        assert_eq!(h.id, "agent-child9");
        assert_eq!(h.parent_id.as_deref(), Some("parent1"), "tagged with parent id");
        assert_eq!(h.agent_id.as_deref(), Some("child9"), "tagged with agent id");
        assert_eq!(h.workflow, None, "directly-spawned agent has no workflow");
        // A fielded query can scope to one parent's whole forest.
        let scoped = text_search(&dir, "parent_id:parent1 zqsubmarker", 10).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "agent-child9");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Regression for the truncated-fresh hole (B1): a parse error *after* chunk docs were already
    /// flushed into the writer must not leave those docs behind — they carry the full current
    /// `(mtime, size)` stamp, so a later `fts_is_fresh` would skip the session forever and freeze
    /// the truncated head into the index. [`index_session_clean`] (the path both `index_all` and
    /// the sub-agent fold drive) deletes the partial docs on error, dropping the id from the
    /// skip-set so the session is retried next run.
    #[test]
    fn demanded_text_missing_or_truncated_source_discards_partial_publication() {
        let home = IsolatedHome::new();
        struct SpanFailure {
            span: cv_core::Span,
        }
        impl cv_core::Adapter for SpanFailure {
            fn harness(&self) -> cv_core::Harness {
                cv_core::Harness::Claude
            }
            fn roots(&self) -> Vec<PathBuf> {
                vec![]
            }
            fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
                Ok(vec![])
            }
            fn stream(
                &self,
                r: &cv_core::SessionRef,
                _: &cv_core::ParseOptions,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<cv_core::Session> {
                let mut first = cv_core::Message::new(cv_core::Role::User);
                first.content.push(cv_core::Block::Text {
                    text: "zqspanpartial ".repeat(CHUNK_BYTES).into(),
                });
                first.content.push(cv_core::Block::ToolUse {
                    id: "call".into(),
                    name: "Edit".into(),
                    input: serde_json::json!({"file_path":"/span.rs","content":"staged"}).into(),
                    namespace: None,
                });
                let _ = sink.message(first);
                let mut broken = cv_core::Message::new(cv_core::Role::Assistant);
                broken.content.push(cv_core::Block::Text {
                    text: cv_core::Text::Span(self.span.clone()),
                });
                let _ = sink.message(broken);
                let mut late = cv_core::Message::new(cv_core::Role::User);
                late.content.push(cv_core::Block::Text { text: "late".into() });
                let _ = sink.message(late);
                Ok(cv_core::Session::new(&r.id, r.harness))
            }
        }
        for truncated in [false, true] {
            let dir = tmpdir();
            let primary = home.home.join(format!("text-{truncated}.jsonl"));
            std::fs::write(&primary, "scratch primary").unwrap();
            let external = dir.join("span");
            if truncated {
                std::fs::write(&external, "short").unwrap();
            }
            let r = cv_core::SessionRef::new(format!("text-{truncated}"), cv_core::Harness::Claude, primary);
            let adapter = SpanFailure {
                span: cv_core::Span {
                    source: Some(external),
                    offset: 0,
                    len: 8192,
                    escaped: false,
                },
            };
            let (index, f) = open_or_create(&dir).unwrap();
            let mut writer = open_writer(&index, 50_000_000).unwrap();
            let mut events = cv_core::events::EventSink::new(None);
            assert!(index_session_clean(
                &mut writer,
                &f,
                &adapter,
                &r,
                1,
                1,
                Some(&mut events),
                Default::default()
            )
            .is_err());
            assert!(
                !events.into_events().unwrap().is_empty(),
                "fixture stages events before failed demand"
            );
            writer.commit().unwrap();
            assert!(text_search(&dir, "zqspanpartial", 10).unwrap().is_empty());
            assert!(!read_indexed_sigs(&index, &f).unwrap().0.contains_key(&r.id));
            assert!(cv_core::catalog::AffordanceTable::load().counts(&r).is_none());
            assert!(cv_core::events::events_for(r.harness.as_str(), &r.id, None).is_empty());
            assert!(cv_core::events::needs_ingest(
                &r,
                cv_core::events::file_mtime_ns(&r.path)
            ));
            assert!(cv_core::resume::MarkTable::load().get(&r).is_none());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn consumer_error_discards_flushed_docs_and_never_publishes_counts_events_or_mark() {
        use cv_core::{Flow, MessageSink};
        let home = IsolatedHome::new();
        let dir = tmpdir();
        let path = home.home.join("consumer.jsonl");
        std::fs::write(&path, "scratch primary").unwrap();
        let r = sref("consumer", "failure", path.to_string_lossy().into_owned());
        struct ProducesChunks;
        impl cv_core::Adapter for ProducesChunks {
            fn harness(&self) -> cv_core::Harness {
                cv_core::Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(
                &self,
                r: &cv_core::SessionRef,
                _: &cv_core::ParseOptions,
                sink: &mut dyn MessageSink,
            ) -> Result<cv_core::Session> {
                for n in 0..6 {
                    let mut m = cv_core::Message::new(cv_core::Role::Assistant);
                    m.content.push(cv_core::Block::ToolUse {
                        id: format!("call{n}"),
                        name: "Edit".into(),
                        namespace: None,
                        input: serde_json::json!({"file_path":"/scratch/file.rs","new_string":"updated"}).into(),
                    });
                    m.content.push(cv_core::Block::Text {
                        text: "zqconsumer ".repeat(2 * CHUNK_BYTES).into(),
                    });
                    sink.message(m); // Ignore Stop deliberately: guard must suppress late output.
                    if sink.wants_checkpoint(n + 1) {
                        sink.checkpoint(&mut cv_core::stream::Checkpoint::new(n + 1));
                    }
                }
                sink.meta(&cv_core::Session::new(&r.id, r.harness));
                Ok(cv_core::Session::new(&r.id, r.harness))
            }
        }
        struct FailsAfterPrefix {
            inner: cv_core::events::EventSink,
            seen: usize,
            error: Option<anyhow::Error>,
        }
        impl MessageSink for FailsAfterPrefix {
            fn message(&mut self, m: cv_core::Message) -> Flow {
                self.inner.message(m);
                self.seen += 1;
                if self.seen == 3 {
                    self.error = Some(anyhow::anyhow!("consulted Json source changed"));
                }
                Flow::Continue
            }
            fn take_error(&mut self) -> Option<anyhow::Error> {
                self.error.take().or_else(|| self.inner.take_error())
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                true
            }
            fn checkpoint(&mut self, cp: &mut cv_core::stream::Checkpoint) {
                self.inner.checkpoint(cp);
            }
        }
        let mut events = FailsAfterPrefix {
            inner: cv_core::events::EventSink::new(None),
            seen: 0,
            error: None,
        };
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 50_000_000).unwrap();
        let error = index_session_clean(
            &mut writer,
            &f,
            &ProducesChunks,
            &r,
            111,
            4096,
            Some(&mut events),
            Default::default(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "consulted Json source changed");
        assert_eq!(events.seen, 3);
        assert!(
            !events.inner.into_events().unwrap().is_empty(),
            "fixture must stage event rows before failure"
        );
        writer.commit().unwrap();
        assert!(text_search(&dir, "zqconsumer", 10).unwrap().is_empty());
        assert!(!read_indexed_sigs(&index, &f).unwrap().0.contains_key(&r.id));
        assert!(cv_core::catalog::AffordanceTable::load().counts(&r).is_none());
        assert!(cv_core::events::events_for(r.harness.as_str(), &r.id, None).is_empty());
        assert!(cv_core::events::needs_ingest(
            &r,
            cv_core::events::file_mtime_ns(&r.path)
        ));
        assert!(cv_core::resume::MarkTable::load().get(&r).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn failed_parse_leaves_no_partial_docs_behind() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();

        /// Streams more than one chunk's worth of body into the sink (forcing ≥1 mid-stream doc
        /// flush), then fails — the shape of a transcript whose tail is malformed after a healthy
        /// multi-chunk head.
        struct FailsAfterAChunk;
        impl cv_core::Adapter for FailsAfterAChunk {
            fn harness(&self) -> cv_core::Harness {
                cv_core::Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(
                &self,
                _r: &cv_core::SessionRef,
                _opts: &cv_core::ParseOptions,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<cv_core::Session> {
                let mut m = cv_core::Message::new(cv_core::Role::User);
                let body = "zqpartial ".repeat(CHUNK_BYTES / 8); // > CHUNK_BYTES → flushed as the message ends
                m.content.push(cv_core::Block::Text { text: body.into() });
                let _ = sink.message(m);
                anyhow::bail!("malformed line mid-transcript")
            }
        }

        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 50_000_000).unwrap();
        let r = sref("victim", "doomed", "/nonexistent/victim.jsonl".into());
        let res = index_session_clean(
            &mut writer,
            &f,
            &FailsAfterAChunk,
            &r,
            111,
            4096,
            None,
            cv_core::events::Provenance::default(),
        );
        assert!(res.is_err(), "the fixture adapter must fail");
        // index_all commits after the error (it continues the sweep); mirror that.
        writer.commit().unwrap();

        // No ghost docs: the flushed chunk must be gone…
        assert!(
            text_search(&dir, "zqpartial", 10).unwrap().is_empty(),
            "partial chunk docs from a failed parse must not be committed (B1)"
        );
        // …and the id is absent from the skip-set, so the next incremental pass retries it
        // instead of skipping a truncation stamped fresh.
        let (sigs, _) = read_indexed_sigs(&index, &f).unwrap();
        assert!(
            !sigs.contains_key("victim"),
            "a failed session must not enter the freshness skip-set (B1)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The sweep reads on every core and writes on one: over many sessions, two of them files under
    /// one id, it leaves exactly the documents the one-at-a-time path does.
    #[test]
    fn the_parallel_sweep_indexes_what_the_sequential_path_does() {
        let _home = IsolatedHome::new();
        let (seq, par, sdir, dup) = (tmpdir(), tmpdir(), tmpdir(), tmpdir());
        let mut refs: Vec<_> = (0..40)
            .map(|i| {
                let id = format!("s{i}");
                sref(&id, "t", write_claude(&sdir, &id, &format!("zqword{i} common")))
            })
            .collect();
        let mut newer = sref("s0", "t", write_claude(&dup, "s0", "zqdupword common"));
        newer.updated_at = chrono::DateTime::from_timestamp(3, 0);
        refs.push(newer.clone());
        let mut canonical = refs[..40].to_vec();
        canonical[0] = newer;
        index_refs(&seq, &canonical, false).unwrap();
        sweep(&par, refs.clone(), true, false).unwrap();
        let docs = |d: &Path| {
            let (index, f) = open_existing(d).unwrap();
            let n = index.reader().unwrap().searcher().num_docs();
            let mut ids: Vec<String> = read_indexed_sigs(&index, &f).unwrap().0.into_keys().collect();
            ids.sort();
            (n, ids)
        };
        assert_eq!(docs(&seq), docs(&par));
        assert_eq!(
            docs(&par).0,
            2 * 40,
            "a body and a meta doc per logical ID, from its preferred copy"
        );
        for q in (0..40)
            .map(|i| format!("zqword{i}"))
            .chain(["zqdupword".into(), "common".into()])
        {
            let ids = |d: &Path| {
                let mut v: Vec<String> = text_search(d, &q, 100).unwrap().into_iter().map(|h| h.id).collect();
                v.sort();
                v
            };
            assert_eq!(ids(&seq), ids(&par), "{q}");
        }
        for d in [seq, par, sdir, dup] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Equal scores rank by session id, whatever order the index was written in.
    #[test]
    fn ties_rank_by_session_id_not_by_write_order() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let refs: Vec<_> = ["zz", "aa", "mm"]
            .iter()
            .map(|id| sref(id, "t", write_claude(&sdir, id, "zqtied identical body")))
            .collect();
        index_refs(&dir, &refs, false).unwrap();
        let ids: Vec<String> = text_search(&dir, "zqtied", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids, ["aa", "mm", "zz"]);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Regression (B2): an FTS-fresh sub-agent whose *catalog* is stale (e.g. first run after an
    /// event-schema upgrade) takes the catalog-only pass — which must never
    /// delete its tantivy docs. The pre-fix path issued the delete-by-id BEFORE the fresh check,
    /// so the agent's docs were dropped and never re-added: a window of silently missing results.
    #[test]
    fn fts_fresh_events_stale_subagent_keeps_its_docs() {
        let h = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let pp = write_claude(&sdir, "parent2", "orchestrator planning the work");
        write_subagent(&pp, "kid2", "zqkeepme lives only in the sub-agent");
        let parent = sref("parent2", "parent session", pp);

        // Fold the forest: sub-agent indexed AND its events stamped in the catalog.
        let folded = index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert_eq!(folded, 1);
        assert_eq!(text_search(&dir, "zqkeepme", 10).unwrap().len(), 1);

        // Invalidate only the event catalog. Keep the independent FTS dependency baselines:
        // losing those would correctly require a whole-source FTS reread too.
        let conn = rusqlite::Connection::open(h.home.join("catalog.db")).unwrap();
        conn.execute_batch(
            "DELETE FROM events; DELETE FROM event_sync;
            DELETE FROM source_dependencies WHERE consumer='events';",
        )
        .unwrap();
        assert!(cv_core::events::needs_ingest(
            &parent,
            cv_core::offsets::file_sig(&parent.path).0
        ));
        let child = cv_core::subagent_tree_of(&parent).remove(0).session;
        assert!(cv_core::events::needs_ingest(
            &child,
            cv_core::offsets::file_sig(&child.path).0
        ));

        // The production `--subagents` sweep: parent and agent are both FTS-fresh, events-stale.
        assert_eq!(
            cv_core::subagent_tree_of(&parent).len(),
            1,
            "fixture must discover exactly one sub-agent"
        );
        let changed = sweep(&dir, vec![parent.clone()], false, true).unwrap().changed;
        assert_eq!(changed, 0, "catalog-only pass must not rewrite tantivy docs");
        assert!(!cv_core::events::needs_ingest(
            &parent,
            cv_core::offsets::file_sig(&parent.path).0
        ));
        let child_source = cv_core::source::Probe::default().snapshot(&child);
        let child_provenance = cv_core::events::Provenance {
            parent_id: Some(parent.id.clone()),
            agent_id: Some("kid2".into()),
            workflow: None,
        };
        assert!(!cv_core::events::SyncTable::load().needs_ingest_with_provenance(
            &child,
            &child_source,
            &child_provenance,
            &mut cv_core::source::Probe::default()
        ));

        assert_eq!(
            text_search(&dir, "zqkeepme", 10).unwrap().len(),
            1,
            "an FTS-fresh, events-stale sub-agent must keep its docs (B2)"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// A catalog wipe removes FTS's independent dependency baselines too. Existing documents
    /// remain searchable, but neither parent nor child can skip their next source pass.
    #[test]
    fn missing_search_dependency_baselines_force_one_incremental_reread() {
        let h = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let pp = write_claude(&sdir, "lost-baseline-parent", "orchestrator planning the work");
        write_subagent(&pp, "lost-kid", "zqbaselinechild lives only in the sub-agent");
        let parent = sref("lost-baseline-parent", "parent session", pp);
        assert_eq!(
            index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap(),
            1
        );
        assert_eq!(sweep(&dir, vec![parent.clone()], false, true).unwrap().changed, 0);
        std::fs::remove_file(h.home.join("catalog.db")).unwrap();
        std::fs::remove_file(h.home.join("catalog.db-wal")).ok();
        std::fs::remove_file(h.home.join("catalog.db-shm")).ok();
        assert_eq!(text_search(&dir, "zqbaselinechild", 10).unwrap().len(), 1);
        assert_eq!(
            sweep(&dir, vec![parent.clone()], false, true).unwrap().changed,
            2,
            "both parent and child need new dependency baselines"
        );
        assert_eq!(text_search(&dir, "zqbaselinechild", 10).unwrap().len(), 1);
        assert_eq!(
            sweep(&dir, vec![parent], false, true).unwrap().changed,
            0,
            "one incremental reread establishes the new baselines"
        );
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(sdir).ok();
    }

    /// Regression (B3): a plain (no `--subagents`) refresh after `cv index --subagents` must NOT
    /// reap the folded forest. Only a --subagents sweep walks the forest, so only it can tell a
    /// vanished agent transcript from a merely-unvisited one.
    #[test]
    fn plain_reindex_keeps_the_folded_subagent_forest() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let pp = write_claude(&sdir, "parent3", "orchestrator text here");
        write_subagent(&pp, "kid3", "zqforest lives only in the sub-agent");
        let parent = sref("parent3", "parent session", pp);

        index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert_eq!(text_search(&dir, "zqforest", 10).unwrap().len(), 1);

        // A plain incremental refresh over the same top-level refs: the parent is fresh-skipped
        // and the deliberately-folded forest must survive the reap.
        let changed = index_refs_incremental(&dir, std::slice::from_ref(&parent)).unwrap();
        assert_eq!(changed, 0, "unchanged parent must be skipped");
        assert_eq!(
            text_search(&dir, "zqforest", 10).unwrap().len(),
            1,
            "a plain refresh must not silently reap the folded sub-agent forest (B3)"
        );
        assert_eq!(text_search(&dir, "orchestrator", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn adopted_agent_copies_have_one_stable_authority_and_parent() {
        let home = IsolatedHome::new();
        let dir = tmpdir();
        let a = sref(
            "parent-a",
            "parent a",
            write_claude_msgs(&home.home, "parent-a", &[("user", "parent a")]),
        );
        let b = sref(
            "parent-b",
            "parent b",
            write_claude_msgs(&home.home, "parent-b", &[("user", "parent b")]),
        );
        write_subagent(a.path.to_str().unwrap(), "shared", "sapphire agent a");
        write_subagent(b.path.to_str().unwrap(), "shared", "amber agent b");
        let first = sweep(&dir, vec![b.clone(), a.clone()], true, true).unwrap();
        assert_eq!(first.changed, 3);
        let (index, fields) = open_existing(&dir).unwrap();
        let meta = meta_doc(&index.reader().unwrap().searcher(), &fields, "agent-shared").unwrap();
        assert_eq!(
            meta.get_first(fields.parent_id).and_then(|v| v.as_str()),
            Some("parent-a")
        );
        assert!(text_search_ids(&dir, "amber").unwrap().is_empty());
        assert_eq!(sweep(&dir, vec![a.clone(), b.clone()], false, true).unwrap().changed, 0);
        let mut top = sref(
            "agent-shared",
            "direct copy",
            write_claude_msgs(&home.home, "direct-copy", &[("user", "topaz direct agent")]),
        );
        top.updated_at = chrono::DateTime::from_timestamp(10, 0);
        assert_eq!(
            sweep(&dir, vec![a.clone(), b.clone(), top.clone()], false, true)
                .unwrap()
                .changed,
            1
        );
        let (index, fields) = open_existing(&dir).unwrap();
        let meta = meta_doc(&index.reader().unwrap().searcher(), &fields, "agent-shared").unwrap();
        assert!(meta.get_first(fields.parent_id).is_none());
        assert_eq!(
            text_search_ids(&dir, "topaz").unwrap(),
            HashSet::from(["agent-shared".to_owned()])
        );
        assert_eq!(
            sweep(&dir, vec![top.clone(), b.clone(), a.clone()], false, true)
                .unwrap()
                .changed,
            0
        );
        std::fs::remove_file(top.path).unwrap();
        assert_eq!(sweep(&dir, vec![b, a], false, true).unwrap().changed, 1);
        assert!(text_search_ids(&dir, "topaz").unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_path_folded_and_direct_authority_refresh_event_parent_both_ways() {
        let home = IsolatedHome::new();
        let dir = tmpdir();
        let parent = sref(
            "parent",
            "parent",
            write_claude_msgs(&home.home, "parent", &[("user", "parent prompt")]),
        );
        let child = write_subagent(parent.path.to_str().unwrap(), "shared", "placeholder");
        std::fs::write(
            &child,
            format!(
                "{}\n",
                serde_json::json!({
                    "type":"assistant","uuid":"edit","sessionId":"agent-shared",
                    "timestamp":"2026-01-01T00:00:00Z","cwd":"/copy-authority",
                    "message":{"role":"assistant","model":"test","content":[{"type":"tool_use",
                        "id":"edit","name":"Edit","input":{"file_path":"/copy-authority/touched.rs",
                            "old_string":"before","new_string":"after"}}]}
                })
            ),
        )
        .unwrap();
        let direct = cv_core::subagent_tree_of(&parent).pop().unwrap().session;
        let attribution = || -> Option<String> {
            rusqlite::Connection::open(home.home.join("catalog.db"))
                .unwrap()
                .query_row(
                    "SELECT parent_id FROM event_sessions WHERE harness='claude' AND session_id='agent-shared'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        sweep(&dir, vec![parent.clone()], true, true).unwrap();
        assert_eq!(attribution().as_deref(), Some("parent"));
        assert_eq!(cv_core::events::events_for("claude", "agent-shared", None).len(), 1);
        assert_eq!(
            sweep(&dir, vec![parent.clone(), direct.clone()], false, true)
                .unwrap()
                .changed,
            1
        );
        assert_eq!(
            attribution(),
            None,
            "direct authority must remove folded attribution on the same source"
        );
        assert_eq!(
            sweep(&dir, vec![direct.clone(), parent.clone()], false, true)
                .unwrap()
                .changed,
            0
        );
        assert_eq!(sweep(&dir, vec![parent.clone()], false, true).unwrap().changed, 1);
        assert_eq!(
            attribution().as_deref(),
            Some("parent"),
            "folded authority must restore its parent"
        );
        assert_eq!(sweep(&dir, vec![parent.clone()], false, true).unwrap().changed, 0);
        assert_eq!(cv_core::events::events_for("claude", "agent-shared", None).len(), 1);
        // Another actual producer can publish direct attribution without touching FTS.
        cv_core::events::ingest_ref(&direct).unwrap();
        assert_eq!(attribution(), None);
        let restored = sweep(&dir, vec![parent.clone()], false, true).unwrap();
        assert_eq!(restored.changed, 0, "event-only authority repair preserves FTS");
        assert!(restored.work.callbacks > 0);
        assert_eq!(attribution().as_deref(), Some("parent"));
        assert_eq!(
            sweep(&dir, vec![parent], false, true).unwrap().work,
            SweepWork::default()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The forest fold also tees sub-agent events into the catalog WITH provenance: a file the
    /// sub-agent edited surfaces in `cv touched`, attributed to the agent + its parent.
    #[test]
    fn subagent_events_ride_along_with_provenance() {
        let h = IsolatedHome::new();
        let dir = tmpdir();

        let pp = write_claude_msgs(&h.home, "evt-parent", &[("user", "spawn an agent to touch a file")]);
        // A sub-agent whose assistant turn edits a file.
        let stem = Path::new(&pp).file_stem().unwrap().to_str().unwrap().to_string();
        let sub_dir = Path::new(&pp).parent().unwrap().join(&stem).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_path = sub_dir.join("agent-ed1.jsonl");
        // Absolute target so the catalog query is unambiguous (the agent transcript carries no cwd
        // of its own, so a relative path wouldn't absolutize the same way the query side does).
        let line = serde_json::json!({
            "type": "assistant", "uuid": "agent-ed1", "sessionId": "agent-ed1",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Edit",
                 "input": {"file_path": "/repo/src/only_subagent.rs", "old_string": "a", "new_string": "b"}}
            ]}
        });
        std::fs::write(&sub_path, format!("{line}\n")).unwrap();

        let mut parent = sref("evt-parent", "parent", pp);
        parent.cwd = Some("/repo".into());

        index_refs_with_subagents(&dir, std::slice::from_ref(&parent), true).unwrap();

        // The sub-agent's edit is in the catalog, attributed to it + its parent.
        let touched = cv_core::events::sessions_touching("/repo/src/only_subagent.rs", true);
        assert_eq!(touched.len(), 1, "sub-agent edit must be catalogued");
        let t = &touched[0];
        assert_eq!(t.session_id, "agent-ed1");
        assert_eq!(t.parent_id.as_deref(), Some("evt-parent"));
        assert_eq!(t.agent_id.as_deref(), Some("ed1"));

        // And the per-session events carry it too.
        let rows = cv_core::events::events_for("claude", "agent-ed1", None);
        let edit = rows.iter().find(|e| e.kind == "file_edit").expect("edit event");
        assert_eq!(edit.parent_id.as_deref(), Some("evt-parent"));
        assert_eq!(edit.agent_id.as_deref(), Some("ed1"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The preview rides only the session's *first* chunk doc, but a hit can score on a *later*
    /// chunk — the fallback must still find the preview via the by-id lookup. Also caps length.
    #[test]
    fn preview_fallback_found_even_when_hit_is_a_late_chunk() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // Marker only in the trailing chunk, so the sole matching doc has no preview field.
        let mut body = "padding ".repeat(700_000);
        assert!(body.len() > CHUNK_BYTES, "fixture must exceed one chunk");
        body.push_str("zqxmarker");
        let p = write_claude(&sdir, "bigone", &body);
        index_refs(&dir, &[sref("bigone", "huge doomed session", p.clone())], false).unwrap();

        std::fs::remove_file(&p).unwrap();

        let hits = text_search(&dir, "zqxmarker", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "bigone");
        assert!(
            hits[0].snippet.starts_with("padding padding"),
            "the preview the meta doc stores expected: {:?}",
            hits[0].snippet
        );
        assert!(hits[0].snippet.ends_with("(source file moved)"));
        // The stored preview itself is capped (marker text rides on top of it).
        let stored = hits[0].snippet.trim_end_matches(" (source file moved)").chars().count();
        assert!(stored <= PREVIEW_CHARS, "preview over cap: {stored} chars");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// A hit shows the session's current label, from its meta doc, even when the body doc that
    /// scored was written before the label was known (here a chunk flushed before the first user
    /// message, the label's source, arrived).
    #[test]
    fn a_hit_is_labelled_by_the_meta_doc_not_by_the_chunk_that_scored() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        // The marker opens the first 4 MB chunk, flushed before the user message arrives.
        let early = format!("zqearly {}", "padding ".repeat(700_000));
        let p = write_claude_msgs(&sdir, "late", &[("assistant", &early), ("user", "zqlabel hello")]);
        index_refs(&dir, &[sref_untitled("late", p)], false).unwrap();
        let hits = text_search(&dir, "zqearly", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("zqlabel hello"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// The claude transcripts and plain codex rollouts of the cv-core corpus: the adapters whose
    /// passes resume.
    fn resumable_corpus() -> Vec<(cv_core::Harness, std::path::PathBuf)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cv-core/tests/corpus");
        fn files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
            entries.sort();
            for p in entries {
                if p.is_dir() {
                    files(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
        let mut out = Vec::new();
        for (dir, h) in [("claude", cv_core::Harness::Claude), ("codex", cv_core::Harness::Codex)] {
            let mut paths = Vec::new();
            files(&root.join(dir), &mut paths);
            for p in paths {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                let top = match h {
                    cv_core::Harness::Claude => p
                        .parent()
                        .and_then(|d| d.parent())
                        .is_some_and(|d| d.ends_with("projects")),
                    _ => name.starts_with("rollout-"),
                };
                if top && name.ends_with(".jsonl") {
                    out.push((h, p));
                }
            }
        }
        out
    }

    /// A copy of each of `n` corpus transcripts (the larger ones, alternating claude and codex)
    /// under `dir`, with the refs a discovery would hand the sweep.
    fn resumable_fleet(dir: &Path, n: usize) -> Vec<(cv_core::SessionRef, Vec<u8>)> {
        let mut all = resumable_corpus();
        all.sort_by_key(|(_, p)| std::cmp::Reverse(std::fs::metadata(p).unwrap().len()));
        let (claude, codex): (Vec<_>, Vec<_>) = all.into_iter().partition(|(h, _)| *h == cv_core::Harness::Claude);
        claude
            .into_iter()
            .zip(codex)
            .flat_map(|(a, b)| [a, b])
            .take(n)
            .map(|(h, src)| {
                // Independent source projects: appending one session must not also change a
                // sibling consulted by another session's conditional fork inference.
                let project = dir.join(src.file_stem().unwrap());
                std::fs::create_dir_all(&project).unwrap();
                let p = project.join(src.file_name().unwrap());
                let bytes = std::fs::read(&src).unwrap();
                let r = cv_core::SessionRef {
                    id: p.file_stem().unwrap().to_string_lossy().into_owned(),
                    harness: h,
                    path: p,
                    cwd: Some("/home/u/proj".into()),
                    title: None,
                    created_at: chrono::DateTime::from_timestamp(1, 0),
                    updated_at: chrono::DateTime::from_timestamp(2, 0),
                    message_count: 1,
                };
                (r, bytes)
            })
            .collect()
    }

    /// Words of the sessions' text, every few: terms a resumed and a whole index must agree on.
    fn some_words(refs: &[cv_core::SessionRef]) -> Vec<String> {
        let mut words = std::collections::BTreeSet::new();
        for r in refs {
            let s = cv_core::harness::for_harness(r.harness).unwrap().parse(r).unwrap();
            let text: String = s.messages.iter().filter_map(|m| m.text()).collect::<Vec<_>>().join(" ");
            for w in text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 5)
                .step_by(23)
            {
                words.insert(w.to_lowercase());
            }
        }
        words.into_iter().collect()
    }

    /// Everything a sweep leaves for `refs`, comparably: every live doc's stored fields (sorted),
    /// the hits for each word, and each session's events and affordance counts.
    fn observe(dir: &Path, refs: &[cv_core::SessionRef], words: &[String]) -> Vec<String> {
        let (index, _) = open_existing(dir).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let mut docs = Vec::new();
        for seg in searcher.segment_readers() {
            let store = seg.get_store_reader(0).unwrap();
            for d in seg.doc_ids_alive() {
                let doc: TantivyDocument = store.get(d).unwrap();
                docs.push(tantivy::schema::document::Document::to_json(&doc, &index.schema()));
            }
        }
        docs.sort();
        let mut out = docs;
        for w in words {
            let mut hits: Vec<String> = text_search(dir, w, 100)
                .unwrap()
                .into_iter()
                .map(|h| format!("{} {:?} {:?} {:?}", h.id, h.title, h.updated_at, h.snippet))
                .collect();
            hits.sort();
            out.push(format!("{w}: {hits:?}"));
        }
        let counts = cv_core::catalog::AffordanceTable::load();
        for r in refs {
            out.push(format!(
                "{:?}",
                cv_core::events::events_for(r.harness.as_str(), &r.id, None)
            ));
            out.push(format!("{:?}", counts.counts(r)));
        }
        out
    }

    /// A whole sweep of the files as they are now, into a fresh index and catalog.
    fn whole_sweep(refs: &[cv_core::SessionRef], words: &[String]) -> Vec<String> {
        whole_sweep_with_work(refs, words).0
    }

    fn whole_sweep_with_work(refs: &[cv_core::SessionRef], words: &[String]) -> (Vec<String>, SweepWork) {
        let (dir, home) = (tmpdir(), tmpdir());
        let prior = std::env::var_os("CLUSTERVISION_HOME");
        std::env::set_var("CLUSTERVISION_HOME", &home);
        let swept = sweep(&dir, refs.to_vec(), false, false).unwrap();
        assert_eq!((swept.resumed, swept.errors), (0, 0));
        let seen = observe(&dir, refs, words);
        std::env::set_var("CLUSTERVISION_HOME", prior.unwrap());
        for d in [dir, home] {
            std::fs::remove_dir_all(d).ok();
        }
        (seen, swept.work)
    }

    /// Write the first `num/den` of each transcript, cut inside a line as a live writer leaves it.
    fn write_heads(fleet: &[(cv_core::SessionRef, Vec<u8>)], num: usize, den: usize) {
        for (r, bytes) in fleet {
            std::fs::write(&r.path, &bytes[..bytes.len() * num / den]).unwrap();
        }
    }

    /// The append case end to end: a sweep over the heads of claude and codex transcripts, the
    /// rest appended, a second sweep that resumes each from its mark — and the index, events and
    /// counts it leaves are exactly a whole sweep's of the grown files. A third append resumes
    /// from the second sweep's marks the same way.
    #[test]
    fn a_resumed_sweep_leaves_what_a_whole_sweep_does() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let fleet = resumable_fleet(&sdir, 6);
        let refs: Vec<_> = fleet.iter().map(|(r, _)| r.clone()).collect();
        write_heads(&fleet, 1, 3);
        let fresh = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!((fresh.resumed, fresh.errors), (0, 0));
        assert!(fresh.work.callbacks > 0 && fresh.work.body_bytes > 0 && fresh.work.documents > 0);
        assert!(fresh.work.writer_opened);
        let warm = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!(warm.errors, 0);
        assert_eq!(
            warm.work,
            SweepWork::default(),
            "unchanged refresh performs no parse or writer work"
        );
        let marks = cv_core::resume::MarkTable::load();
        assert!(refs.iter().all(|r| marks.get(r).is_some()), "every head left a mark");
        for (num, den) in [(2, 3), (1, 1)] {
            write_heads(&fleet, num, den);
            let swept = sweep(&dir, refs.clone(), false, false).unwrap();
            assert_eq!(swept.errors, 0);
            assert_eq!(
                (swept.changed, swept.resumed),
                (refs.len(), refs.len()),
                "at {num}/{den}"
            );
            let words = some_words(&refs);
            assert!(words.len() > 50, "{} words", words.len());
            let resumed = observe(&dir, &refs, &words);
            let (whole, work) = whole_sweep_with_work(&refs, &words);
            assert_eq!(resumed, whole, "at {num}/{den}");
            assert!(
                swept.work.callbacks < work.callbacks,
                "suffix callbacks {:?} versus whole {:?}",
                swept.work,
                work
            );
            assert!(
                swept.work.body_bytes < work.body_bytes,
                "suffix searchable bytes {:?} versus whole {:?}",
                swept.work,
                work
            );
            assert!(
                swept.work.documents <= work.documents,
                "suffix document submissions {:?} versus whole {:?}",
                swept.work,
                work
            );
        }
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Native Qwen has stable ChatRecord boundaries; legacy replay can replace old IDs. Keep
    /// this end-to-end check across counts, events and stored search docs, including partial EOF.
    #[test]
    fn qwen_native_append_resumes_but_legacy_replay_and_metadata_changes_do_not() {
        use cv_core::{Harness, SessionRef};
        let _home = IsolatedHome::new();
        let (dir, root) = (tmpdir(), tmpdir());
        let chats = root.join("projects/group/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let r = SessionRef::new("native", Harness::Qwen, chats.join("native.jsonl"));
        let rec = |id: &str, body: serde_json::Value| {
            let mut v = serde_json::json!({"uuid":id,"sessionId":"native","cwd":"/scratch",
                "timestamp":"2026-10-11T00:00:01Z","version":"0.25.1"});
            v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
            v.to_string()
        };
        let mut text = String::new();
        for n in 0..100 {
            text.push_str(&rec(&format!("u{n}"), serde_json::json!({"type":"user",
                "provenance":"real_user","message":{"role":"user","parts":[{"text":format!("prefix sapphire message {n}")}]}})));
            text.push('\n');
        }
        text.push_str(&rec(
            "a1",
            serde_json::json!({"type":"assistant","model":"first-model",
            "message":{"role":"model","parts":[{"functionCall":{"id":"call","name":"tool_call",
                "args":{"name":"agent","arguments":{"prompt":"explore"}}}}]}}),
        ));
        text.push('\n');
        // Valid EOF records emit, but may not be checkpointed until the physical newline lands.
        text.push_str(&rec(
            "pending",
            serde_json::json!({"type":"assistant","model":"next-model",
            "message":{"role":"model","parts":[{"text":"pending amber reply"}]}}),
        ));
        std::fs::write(&r.path, &text).unwrap();
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().resumed, 0);
        assert!(
            cv_core::resume::MarkTable::load().get(&r).is_some(),
            "native head must leave a safe chunk boundary"
        );
        let words: Vec<_> = ["sapphire", "amber", "returned", "compressed"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for suffix in [
            rec(
                "result",
                serde_json::json!({"type":"tool_result","message":{"role":"user","parts":[
                {"functionResponse":{"id":"call","name":"tool_call","response":{"output":"returned emerald"}}}]},
                "toolCallResult":{"callId":"call","status":"success"}}),
            ),
            rec(
                "compact",
                serde_json::json!({"type":"system","subtype":"chat_compression","systemPayload":{
                "info":{"originalTokenCount":99,"newTokenCount":9},"compressedHistory":[{"role":"user","parts":[{"text":"compressed ruby"}]}]}}),
            ),
        ] {
            text.push('\n');
            text.push_str(&suffix);
            text.push('\n');
            std::fs::write(&r.path, &text).unwrap();
            let swept = sweep(&dir, vec![r.clone()], false, false).unwrap();
            assert_eq!((swept.changed, swept.resumed), (1, 1));
            assert_eq!(
                observe(&dir, std::slice::from_ref(&r), &words),
                whole_sweep(std::slice::from_ref(&r), &words)
            );
        }

        // A sidecar edit must not reuse checkpointed lineage, even alongside transcript growth.
        let sub = root.join("projects/group/subagents/native");
        std::fs::create_dir_all(&sub).unwrap();
        let child = SessionRef::new("agent-worker", Harness::Qwen, sub.join("agent-worker.jsonl"));
        let meta = sub.join("agent-worker.meta.json");
        std::fs::write(&meta, r#"{"toolUseId":"old","subagentName":"worker"}"#).unwrap();
        std::fs::write(&child.path, &text).unwrap();
        sweep(&dir, vec![r.clone(), child.clone()], false, false).unwrap();
        std::fs::write(&meta, r#"{"toolUseId":"new","subagentName":"worker"}"#).unwrap();
        std::fs::write(
            &child.path,
            format!(
                "{text}\n{}\n",
                rec(
                    "last",
                    serde_json::json!({"type":"user",
            "message":{"role":"user","parts":[{"text":"child append"}]}})
                )
            ),
        )
        .unwrap();
        let swept = sweep(&dir, vec![r.clone(), child.clone()], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed), (1, 0));
        assert_eq!(
            observe(&dir, &[r.clone(), child.clone()], &words),
            whole_sweep(&[r, child], &words)
        );

        let legacy_dir = root.join("tmp/hash/chats");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = SessionRef::new("legacy", Harness::Qwen, legacy_dir.join("session-legacy.jsonl"));
        let mut old = String::from(
            r#"{"sessionId":"legacy","startTime":"2026-10-11T00:00:00Z","lastUpdated":"2026-10-11T00:00:01Z","kind":"main"}"#,
        );
        for n in 0..60 {
            old.push_str(&format!(
                "\n{{\"id\":\"u{n}\",\"type\":\"user\",\"content\":\"legacy prompt\"}}"
            ));
        }
        old.push_str("\n{\"id\":\"answer\",\"type\":\"gemini\",\"content\":\"obsolete sapphire\"}\n");
        std::fs::write(&legacy.path, &old).unwrap();
        sweep(&dir, vec![legacy.clone()], false, false).unwrap();
        old.push_str("{\"id\":\"answer\",\"type\":\"gemini\",\"content\":\"replacement amber\"}\n");
        std::fs::write(&legacy.path, &old).unwrap();
        let swept = sweep(&dir, vec![legacy.clone()], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed), (1, 0));
        assert_eq!(
            observe(&dir, std::slice::from_ref(&legacy), &words),
            whole_sweep(std::slice::from_ref(&legacy), &words)
        );
        assert!(text_search(&dir, "sapphire", 100).unwrap().is_empty());
        assert!(!text_search(&dir, "amber", 100).unwrap().is_empty());
        for d in [dir, root] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn fts_only_reread_keeps_event_checkpoints_for_later_append() {
        let home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let fleet = resumable_fleet(&sdir, 2);
        let refs: Vec<_> = fleet.iter().map(|(r, _)| r.clone()).collect();
        write_heads(&fleet, 1, 3);
        sweep(&dir, refs.clone(), false, false).unwrap();
        let conn = rusqlite::Connection::open(home.home.join("catalog.db")).unwrap();
        conn.execute(
            "DELETE FROM source_dependencies WHERE consumer=?1",
            [source_consumer(&dir).unwrap()],
        )
        .unwrap();
        for r in &refs {
            assert!(!cv_core::events::needs_ingest(
                r,
                cv_core::events::file_mtime_ns(&r.path)
            ));
        }
        let reread = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!((reread.changed, reread.resumed), (refs.len(), 0));
        write_heads(&fleet, 1, 1);
        let appended = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!(
            appended.resumed,
            refs.len(),
            "FTS-only rereads must keep event checkpoint coverage"
        );
        let words = some_words(&refs);
        assert_eq!(observe(&dir, &refs, &words), whole_sweep(&refs, &words));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn failed_event_publication_cannot_advance_a_resume_mark_on_retry() {
        let home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let fleet = resumable_fleet(&sdir, 2);
        let refs: Vec<_> = fleet.iter().map(|(r, _)| r.clone()).collect();
        write_heads(&fleet, 1, 3);
        sweep(&dir, refs.clone(), false, false).unwrap();
        assert!(refs.iter().all(|r| cv_core::resume::MarkTable::load().get(r).is_some()));
        let before: Vec<_> = refs
            .iter()
            .map(|r| format!("{:?}", cv_core::events::events_for(r.harness.as_str(), &r.id, None)))
            .collect();
        let conn = rusqlite::Connection::open(home.home.join("catalog.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'test'); END;",
        )
        .unwrap();
        write_heads(&fleet, 1, 1);
        let grown = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!(grown.resumed, refs.len());
        assert_eq!(grown.errors, refs.len(), "publication failures must be reported");
        assert!(grown
            .first_error
            .as_deref()
            .unwrap()
            .contains("event publication failed"));
        assert!(
            grown.first_error.as_deref().unwrap().contains("test"),
            "retain the SQLite error reason"
        );
        // FTS commits successfully, but the event suffix transaction rolls back and its mark
        // must be removed. An events-only retry over an unchanged source must read whole.
        for (r, expected) in refs.iter().zip(&before) {
            assert_eq!(
                format!("{:?}", cv_core::events::events_for(r.harness.as_str(), &r.id, None)),
                *expected
            );
            assert!(cv_core::events::needs_ingest(
                r,
                cv_core::events::file_mtime_ns(&r.path)
            ));
            assert!(cv_core::resume::MarkTable::load().get(r).is_none());
            let source = cv_core::source::Probe::default().snapshot(r);
            assert!(
                cv_core::catalog::AffordanceTable::load()
                    .counts_with_source(r, &source)
                    .is_some(),
                "event rollback must not prevent independent counts publication on the reused connection"
            );
        }
        conn.execute_batch("DROP TRIGGER reject_event;").unwrap();
        let retry = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!((retry.resumed, retry.errors), (0, 0));
        for r in &refs {
            assert!(!cv_core::events::needs_ingest(
                r,
                cv_core::events::file_mtime_ns(&r.path)
            ));
        }
        let words = some_words(&refs);
        assert_eq!(observe(&dir, &refs, &words), whole_sweep(&refs, &words));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn discovery_metadata_schema_migration_is_indexing_only() {
        let dir = tmpdir();
        let mut old = serde_json::to_value(build_schema()).unwrap();
        old.as_array_mut()
            .unwrap()
            .retain(|field| field["name"] != "discovery_metadata");
        let old: Schema = serde_json::from_value(old).unwrap();
        let index = Index::create_in_dir(&dir, old.clone()).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        let mut doc = TantivyDocument::default();
        doc.add_text(old.get_field("id").unwrap(), "old");
        doc.add_text(old.get_field("title").unwrap(), "obsolete");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(index);
        // A query cannot silently trust old stale postings, and never clears them.
        assert!(open_existing(&dir).err().unwrap().to_string().contains("cv index"));
        let existing = Index::open_in_dir(&dir).unwrap();
        assert_eq!(existing.reader().unwrap().searcher().num_docs(), 1);
        drop(existing);
        let (new, _) = open_or_create(&dir).unwrap();
        assert!(schema_current(&new.schema()));
        assert_eq!(new.reader().unwrap().searcher().num_docs(), 0);
        drop(new);
        std::fs::remove_dir_all(dir).ok();
    }

    /// A title-changing append cannot keep prefix title postings: Boolean clauses are
    /// evaluated within each body document, not joined through the latest meta document.
    #[test]
    fn discovery_metadata_title_append_matches_whole_index() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let path = write_claude(&sdir, "titles", &format!("prefixneedle {}", "ordinary ".repeat(1000)));
        let mut r = sref("titles", "obsolete", path.clone());
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 1);
        append(
            &path,
            &format!(
                "{}\n",
                serde_json::json!({
                    "type":"user", "uuid":"tail", "message":{"role":"user","content":"tailneedle"}
                })
            ),
        );
        r.title = Some("current".into());
        r.updated_at = chrono::DateTime::from_timestamp(3, 0);
        let swept = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed, swept.errors), (1, 0, 0));
        assert!(text_search(&dir, "title:obsolete", 10).unwrap().is_empty());
        assert!(text_search(&dir, "title:obsolete AND body:prefixneedle", 10)
            .unwrap()
            .is_empty());
        let hits = text_search(&dir, "title:current AND body:prefixneedle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("current"));
        let words = [
            "title:obsolete",
            "title:current",
            "title:current AND body:prefixneedle",
            "tailneedle",
        ]
        .map(str::to_string);
        assert_eq!(
            observe(&dir, std::slice::from_ref(&r), &words),
            whole_sweep(std::slice::from_ref(&r), &words)
        );

        // The next append changes only replaceable updated_at and retains prefix docs.
        append(
            &path,
            &format!(
                "{}\n",
                serde_json::json!({
                    "type":"user", "uuid":"tail2", "message":{"role":"user","content":"nextneedle"}
                })
            ),
        );
        r.updated_at = chrono::DateTime::from_timestamp(4, 0);
        let swept = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed, swept.errors), (1, 1, 0));
        assert_eq!(observe(&dir, &[r.clone()], &words), whole_sweep(&[r], &words));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Discovery inputs can change without changing the transcript tuple (sidecar metadata,
    /// a new parent attribution, or caller-provided metadata). Skip must account for them too.
    #[test]
    fn discovery_metadata_guard_covers_static_fields_and_metadata_only_changes() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let path = write_claude(&sdir, "metadata", "bodyneedle");
        let mut r = sref("metadata", "obsolete", path);
        sweep(&dir, vec![r.clone()], false, false).unwrap();
        r.title = Some("current".into());
        r.cwd = Some("/changed/project".into());
        r.created_at = chrono::DateTime::from_timestamp(8, 0);
        r.updated_at = chrono::DateTime::from_timestamp(9, 0);
        let swept = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed, swept.errors), (1, 0, 0));
        let words = [
            "title:obsolete",
            "title:current AND body:bodyneedle",
            "cwd:changed AND body:bodyneedle",
        ]
        .map(str::to_string);
        assert_eq!(
            observe(&dir, std::slice::from_ref(&r), &words),
            whole_sweep(std::slice::from_ref(&r), &words)
        );
        assert_eq!(sweep(&dir, vec![r.clone()], false, false).unwrap().changed, 0);
        r.updated_at = chrono::DateTime::from_timestamp(10, 0);
        let updated = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((updated.changed, updated.resumed, updated.errors), (1, 0, 0));
        assert_eq!(
            observe(&dir, std::slice::from_ref(&r), &words),
            whole_sweep(std::slice::from_ref(&r), &words)
        );
        let base = DiscoveryMetadata::new(&r, &Default::default());
        let mut alternatives = Vec::new();
        for field in 0..9 {
            let mut changed = base.clone();
            match field {
                0 => changed.id.push('x'),
                1 => changed.harness.push('x'),
                2 => changed.path.push('x'),
                3 => changed.cwd = None,
                4 => changed.title = None,
                5 => changed.created_at = None,
                6 => changed.parent_id = Some("parent".into()),
                7 => changed.agent_id = Some("agent".into()),
                _ => changed.workflow = Some("workflow".into()),
            }
            alternatives.push(changed);
        }
        assert!(alternatives.iter().all(|changed| !base.same_body(changed)));
        let mut empty = base.clone();
        empty.title = Some(String::new());
        let mut absent = empty.clone();
        absent.title = None;
        assert!(!empty.same_body(&absent));
        let mut updated = base.clone();
        updated.updated_at = None;
        assert_ne!(base, updated);
        assert!(base.same_body(&updated));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[cfg(unix)]
    #[test]
    fn primary_identity_restored_rewrite_refreshes_index_events_and_counts() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let path = sdir.join("identity.jsonl");
        let body: String = (0..20).map(|i| {
            let content = if i == 10 {
                serde_json::json!([
                    {"type":"text", "text":"oldneedle"},
                    {"type":"tool_use", "id":"edit", "name":"Read", "input":{"file_path":"/old.rs","old_string":"old","new_string":"new"}},
                    {"type":"tool_use", "id":"todo", "name":"TodoWrite", "input":{"todos":[{"content":"task","status":"pending"}]}}
                ])
            } else { serde_json::json!("ordinary ".repeat(500)) };
            format!("{}\n", serde_json::json!({"type":if i==10 {"assistant"} else {"user"},
                "uuid":format!("u{i}"), "message":{"role":if i==10 {"assistant"} else {"user"}, "content":content}}))
        }).collect();
        std::fs::write(&path, &body).unwrap();
        let r = sref("identity", "stabletitle", path.to_string_lossy().into_owned());
        sweep(&dir, vec![r.clone()], false, false).unwrap();
        let old_counts = cv_core::catalog::AffordanceTable::load().counts(&r).unwrap();
        let old_events = cv_core::events::events_for(r.harness.as_str(), &r.id, None);
        let head = cv_core::resume::MarkTable::load().get(&r).unwrap().clone();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let rewritten = body
            .replace("oldneedle", "newneedle")
            .replace("\"Read\"", "\"Edit\"")
            .replace("TodoWrite", "OtherTool")
            .replace("/old.rs", "/new.rs");
        assert_eq!(body.len(), rewritten.len());
        std::fs::write(&path, rewritten).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(head.holds(&path, body.len() as u64, &head.version));
        let swept = sweep(&dir, vec![r.clone()], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed, swept.errors), (1, 0, 0));
        assert!(text_search(&dir, "oldneedle", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "newneedle", 10).unwrap().len(), 1);
        assert_ne!(
            old_counts,
            cv_core::catalog::AffordanceTable::load().counts(&r).unwrap()
        );
        assert_ne!(
            format!("{old_events:?}"),
            format!("{:?}", cv_core::events::events_for(r.harness.as_str(), &r.id, None))
        );
        let words = ["oldneedle", "newneedle"].map(str::to_string);
        assert_eq!(
            observe(&dir, std::slice::from_ref(&r), &words),
            whole_sweep(std::slice::from_ref(&r), &words)
        );
        let unchanged = sweep(&dir, vec![r], false, false).unwrap();
        assert_eq!((unchanged.changed, unchanged.work.callbacks), (0, 0));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// A same-shaped file renamed over the transcript after a pass read it must not take that
    /// pass's mark: once it grows, resuming would keep the old file's docs for its prefix.
    #[cfg(unix)]
    #[test]
    fn a_file_renamed_over_after_the_pass_does_not_inherit_its_mark() {
        use std::io::Write;
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let path = sdir.join("renamed.jsonl");
        let line = |i: usize, marker: &str| {
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user", "uuid": format!("u{i}"), "sessionId": "renamed",
                    "message": {"role": "user", "content": format!("{} {marker}", "stable payload ".repeat(200))}
                })
            )
        };
        let body = |marker: &str| -> String {
            (0..20)
                .map(|i| line(i, if i == 10 { marker } else { "ordinary" }))
                .collect()
        };
        std::fs::write(&path, body("zqoldword")).unwrap();
        let replacement = sdir.join("replacement.jsonl");
        std::fs::write(&replacement, body("zqnewword")).unwrap();
        let r = sref("renamed", "renamed", path.to_string_lossy().into_owned());
        let pending = replacement.clone();
        *AFTER_PASS.lock().unwrap() = Some(Box::new(move |source| {
            let _ = std::fs::rename(&pending, source);
        }));
        let first = sweep(&dir, vec![r.clone()], false, false);
        *AFTER_PASS.lock().unwrap() = None;
        first.unwrap();
        assert!(!replacement.exists(), "the hook replaced the transcript");
        let head = cv_core::resume::MarkTable::load().get(&r).unwrap().clone();
        let at = body("zqoldword").find("zqoldword").unwrap() as u64;
        assert!(at > 4096 && at + 4096 < head.offset, "the edit is outside both samples");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(line(20, "ordinary").as_bytes())
            .unwrap();
        let swept = sweep(&dir, vec![r], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed), (1, 0));
        assert!(text_search(&dir, "zqoldword", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "zqnewword", 10).unwrap().len(), 1);
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Equal-size edits require a whole pass even when the old checkpoint's samples match.
    #[test]
    fn same_size_rewrite_does_not_resume_from_a_matching_sampled_mark() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let path = sdir.join("edited.jsonl");
        let body: String = (0..20)
            .map(|i| {
                let marker = if i == 10 { "zqoldone" } else { "ordinary" };
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "user", "uuid": format!("u{i}"), "sessionId": "edited",
                        "message": {"role": "user", "content": format!("{} {marker}", "stable payload ".repeat(200))}
                    })
                )
            })
            .collect();
        std::fs::write(&path, &body).unwrap();
        let r = sref("edited", "edited", path.to_string_lossy().into_owned());
        sweep(&dir, vec![r.clone()], false, false).unwrap();
        let head = cv_core::resume::MarkTable::load().get(&r).unwrap().clone();
        let at = body.find("zqoldone").unwrap() as u64;
        assert!(
            at > 4096 && at + 4096 < head.offset,
            "edit is outside both sampled ranges"
        );
        let edited = body.replace("zqoldone", "zqnewone");
        assert_eq!(body.len(), edited.len());
        std::fs::write(&path, edited).unwrap();
        bump_mtime(path.to_str().unwrap());
        assert!(
            head.holds(&path, body.len() as u64, &head.version),
            "sampled mark cannot detect this edit"
        );

        let swept = sweep(&dir, vec![r], false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed), (1, 0));
        assert!(text_search(&dir, "zqoldone", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "zqnewone", 10).unwrap().len(), 1);
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// A mark whose sampled bytes no longer match is never resumed from: an edit near the mark,
    /// the file cut short of it, or a mark another parser version wrote. Each such session is
    /// read whole and leaves what a whole sweep does.
    #[test]
    fn a_stale_mark_falls_back_to_a_whole_pass() {
        let _home = IsolatedHome::new();
        let (dir, sdir) = (tmpdir(), tmpdir());
        let fleet = resumable_fleet(&sdir, 3);
        let refs: Vec<_> = fleet.iter().map(|(r, _)| r.clone()).collect();
        write_heads(&fleet, 1, 2);
        sweep(&dir, refs.clone(), false, false).unwrap();
        let marks = cv_core::resume::MarkTable::load();
        let head = |i: usize| marks.get(&refs[i]).unwrap().clone();

        // 0: a byte before its mark changes (a letter inside the line before it), then it grows.
        let (r0, b0) = &fleet[0];
        let mut bytes = b0.clone();
        let at = (0..head(0).offset as usize - 1)
            .rev()
            .find(|&i| bytes[i].is_ascii_lowercase())
            .unwrap();
        bytes[at] = if bytes[at] == b'q' { b'z' } else { b'q' };
        std::fs::write(&r0.path, &bytes).unwrap();
        // 1: cut short of its mark.
        let (r1, b1) = &fleet[1];
        std::fs::write(&r1.path, &b1[..head(1).offset as usize - 1]).unwrap();
        // 2: grown, but its mark was written by another version.
        let (r2, b2) = &fleet[2];
        std::fs::write(&r2.path, b2).unwrap();
        let cp = cv_core::resume::state(r2, &head(2)).unwrap();
        let other = cv_core::resume::MarkHead {
            version: format!("{}-older", head(2).version),
            ..head(2)
        };
        cv_core::resume::update(&[(r2.clone(), Some((other, cp)))]);

        let swept = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!((swept.changed, swept.resumed), (3, 0));
        let words = some_words(&refs);
        assert_eq!(observe(&dir, &refs, &words), whole_sweep(&refs, &words));
        for d in [dir, sdir] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn unchanged_sweep_does_not_require_writer_lock() {
        let home = IsolatedHome::new();
        let dir = home.home.join("index");
        let refs = vec![cv_core::SessionRef::new(
            "warm",
            cv_core::Harness::Claude,
            write_claude(&home.home, "warm", "zqwarm"),
        )];
        sweep(&dir, refs.clone(), false, false).unwrap();
        let before = observe(&dir, &refs, &["zqwarm".into()]);
        let (index, _) = open_existing(&dir).unwrap();
        let _writer_lock = open_writer(&index, 15_000_000).unwrap();
        let swept = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!(
            (swept.changed, swept.removed, swept.errors, swept.admission.1),
            (0, 0, 0, 0)
        );
        assert_eq!(swept.work, SweepWork::default());
        assert_eq!(observe(&dir, &refs, &["zqwarm".into()]), before);
    }

    #[test]
    fn sweep_plan_rechecks_publication_under_writer_lock() {
        let home = IsolatedHome::new();
        let dir = home.home.join("index");
        let refs = vec![cv_core::SessionRef::new(
            "warm",
            cv_core::Harness::Claude,
            write_claude(&home.home, "warm", "zqwarm"),
        )];
        sweep(&dir, refs.clone(), false, false).unwrap();
        write_claude(&home.home, "warm", "zqchanged warm source");
        let (index, f) = open_existing(&dir).unwrap();
        let planner = SweepPlanner {
            index: &index,
            f: &f,
            dir: &dir,
            refs: &refs,
            rebuild: false,
            subagents: false,
        };
        let metadata = std::fs::read(dir.join("meta.json")).unwrap();
        let plan = planner.plan().unwrap();
        assert_eq!(plan.jobs.len(), 1);
        assert!(plan.missing.is_empty());
        // Another writer publishes a session after our optimistic plan but before acquisition.
        {
            let mut concurrent = open_writer(&index, 15_000_000).unwrap();
            let mut doc = TantivyDocument::default();
            doc.add_text(f.id, "vanished");
            doc.add_text(f.kind, KIND_META);
            doc.add_i64(f.mtime, 1);
            doc.add_i64(f.size, 1);
            concurrent.add_document(doc).unwrap();
            concurrent.commit().unwrap();
        }
        let writer = open_writer(&index, 15_000_000).unwrap();
        let replacement = planner.under_writer(plan, &metadata, &writer).unwrap();
        assert_eq!(replacement.jobs.len(), 1);
        assert_eq!(replacement.missing, ["vanished"]);
        drop(writer);
        let swept = sweep(&dir, refs, false, false).unwrap();
        assert_eq!((swept.changed, swept.removed), (1, 1));
    }

    #[test]
    fn unchanged_sweep_finishes_pending_compaction() {
        let home = IsolatedHome::new();
        let dir = home.home.join("index");
        let refs = vec![cv_core::SessionRef::new(
            "warm",
            cv_core::Harness::Claude,
            write_claude(&home.home, "warm", "zqwarm"),
        )];
        sweep(&dir, refs.clone(), false, false).unwrap();
        let (index, f) = open_existing(&dir).unwrap();
        {
            let mut writer = open_writer(&index, 15_000_000).unwrap();
            for _ in 1..TIER_FANOUT {
                let mut doc = TantivyDocument::default();
                doc.add_text(f.id, "warm");
                doc.add_text(f.kind, KIND_BODY);
                doc.add_text(f.body, "zqpending");
                writer.add_document(doc).unwrap();
                writer.commit().unwrap();
            }
        }
        assert!(index.searchable_segment_ids().unwrap().len() >= TIER_FANOUT);
        let before = observe(&dir, &refs, &["zqwarm".into(), "zqpending".into()]);
        let swept = sweep(&dir, refs.clone(), false, false).unwrap();
        assert_eq!((swept.changed, swept.removed, swept.errors), (0, 0, 0));
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 1);
        assert_eq!(observe(&dir, &refs, &["zqwarm".into(), "zqpending".into()]), before);
    }

    #[test]
    #[ignore = "explicit scratch warm sweep measurement"]
    fn warm_sweep_transaction_probe() {
        let home = IsolatedHome::new();
        let dir = home.home.join("index");
        let mut refs = Vec::new();
        let sessions: usize = std::env::var("CV_FTS_WARM_SESSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(256);
        let cpu = std::env::var_os("CV_FTS_WARM_CPU").is_some();
        let runs = if cpu { 100 } else { 20 };
        for n in 0..sessions {
            let id = format!("warm{n}");
            let path = write_claude(&home.home, &id, "zqwarm common body");
            refs.push(cv_core::SessionRef::new(id, cv_core::Harness::Claude, path));
        }
        assert_eq!(sweep(&dir, refs.clone(), false, false).unwrap().changed, refs.len());
        let expected = (!cpu).then(|| observe(&dir, &refs, &["zqwarm".into()]));
        let meta_before = std::fs::read(dir.join("meta.json")).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..runs {
            let result = sweep(&dir, refs.clone(), false, false).unwrap();
            assert_eq!(
                (result.changed, result.removed, result.errors, result.admission.1),
                (0, 0, 0, 0)
            );
        }
        let millis = started.elapsed().as_secs_f64() * 1000.0;
        if let Some(expected) = expected {
            assert_eq!(observe(&dir, &refs, &["zqwarm".into()]), expected);
        }
        println!(
            "warm sweep: {sessions} sessions x{runs}: {millis:.3}ms, meta_changed={}",
            std::fs::read(dir.join("meta.json")).unwrap() != meta_before
        );
        if cpu {
            return;
        }
        let (index, _) = open_existing(&dir).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let mut writer = open_writer(&index, 50_000_000).unwrap();
            writer.commit().unwrap();
            assert_eq!(compact(&dir, &index, &mut writer).unwrap(), 0);
        }
        println!(
            "writer lifecycle alone x20: {:.3}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert_eq!(
                read_indexed_sigs(&index, &fields_of(&index.schema()).unwrap())
                    .unwrap()
                    .0
                    .len(),
                refs.len()
            );
        }
        println!(
            "indexed signature query alone x20: {:.3}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert_eq!(
                text_search_indexed(&dir, "zqwarm", 100).unwrap().len(),
                sessions.min(100)
            );
        }
        println!(
            "ranked query alone x20: {:.3}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }

    /// One commit per run leaves one small segment per run: the writer itself never merges them
    /// (a background merge outlived by the process is the 11 s / 1 GB `cv index` used to abandon),
    /// so `compact` merges a full tier to one segment, keeps every doc findable, and then has
    /// nothing left to do on an unchanged index.
    #[test]
    fn compact_merges_a_full_tier_once_and_then_rests() {
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        for i in 0..TIER_FANOUT {
            let mut doc = TantivyDocument::default();
            doc.add_text(f.id, format!("s{i}"));
            doc.add_text(f.body, format!("zqtier{i} common"));
            writer.add_document(doc).unwrap();
            writer.commit().unwrap();
        }
        assert_eq!(index.searchable_segment_ids().unwrap().len(), TIER_FANOUT);
        assert_eq!(compact(&dir, &index, &mut writer).unwrap(), 1);
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 1);
        assert_eq!(
            compact(&dir, &index, &mut writer).unwrap(),
            0,
            "an unchanged index merges nothing"
        );
        let hits = text_search(&dir, "common", 50).unwrap();
        assert_eq!(hits.len(), TIER_FANOUT);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[derive(Default, Debug)]
    struct MergeBytes {
        inputs: u64,
        outputs: u64,
        merges: usize,
        cold_rewrites: usize,
        final_segments: usize,
    }
    fn segment_bytes(dir: &Path, meta: &tantivy::index::SegmentMeta) -> u64 {
        meta.list_files()
            .iter()
            .filter_map(|path| std::fs::metadata(dir.join(path)).ok())
            .map(|meta| meta.len())
            .sum()
    }
    fn measured_compact(
        dir: &Path,
        index: &Index,
        writer: &mut Writer,
        floor: u64,
        cold: &mut HashSet<tantivy::index::SegmentId>,
    ) -> MergeBytes {
        let mut work = MergeBytes::default();
        let mut pending: Option<(HashSet<tantivy::index::SegmentId>, bool)> = None;
        compact_with_candidates(dir, index, writer, |dir, index| {
            let metas = index.searchable_segment_metas()?;
            if let Some((before, rewrote_cold)) = pending.take() {
                for meta in metas.iter().filter(|meta| !before.contains(&meta.id())) {
                    work.outputs += segment_bytes(dir, meta);
                    if rewrote_cold {
                        cold.insert(meta.id());
                    }
                }
            }
            let pick = compaction_candidates_at_floor(dir, index, floor)?;
            if let Some(ids) = &pick {
                work.merges += 1;
                let rewrote_cold = ids.iter().any(|id| cold.contains(id));
                work.cold_rewrites += usize::from(rewrote_cold);
                work.inputs += metas
                    .iter()
                    .filter(|meta| ids.contains(&meta.id()))
                    .map(|meta| segment_bytes(dir, meta))
                    .sum::<u64>();
                pending = Some((metas.iter().map(|meta| meta.id()).collect(), rewrote_cold));
            }
            Ok(pick)
        })
        .unwrap();
        work
    }
    fn session_ids_and_reads(dir: &Path, term: &str) -> (Vec<String>, usize) {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let (index, f) = open_existing(dir).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let query = QueryParser::for_index(&index, vec![f.body]).parse_query(term).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let mut collector = crate::session_collector::TopSessions::new(f.id, 16);
        collector.stored_reads = Some(reads.clone());
        let mut ids: Vec<_> = searcher
            .search(&query, &collector)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.id)
            .collect();
        ids.sort();
        (ids, reads.load(Ordering::Relaxed))
    }
    fn append_merge_control(legacy: bool) -> (MergeBytes, Vec<String>, usize) {
        use tantivy::{
            query::{BooleanQuery, TermQuery},
            schema::IndexRecordOption,
        };
        let dir = tmpdir();
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        let mut cold_doc = TantivyDocument::default();
        cold_doc.add_text(f.id, "cold");
        cold_doc.add_text(f.kind, KIND_BODY);
        let words: String = (0..4096).map(|n| format!("coldword{n:08x} ")).collect();
        cold_doc.add_text(f.body, format!("common {words}"));
        writer.add_document(cold_doc).unwrap();
        writer.commit().unwrap();
        let cold_meta = index.searchable_segment_metas().unwrap().pop().unwrap();
        let cold_bytes = segment_bytes(&dir, &cold_meta);
        let floor = cold_bytes / 2;
        eprintln!("append tier scale: cold={cold_bytes} bytes, floor={floor} bytes, legacy={legacy}");
        let mut cold = HashSet::from([cold_meta.id()]);
        let mut total = MergeBytes::default();
        for n in 0..24 {
            // An immutable body chunk and replaceable meta doc per append. Dead meta docs
            // are exactly half of their segment, preserving DEAD_RATIO's strict boundary.
            writer
                .delete_query(Box::new(BooleanQuery::intersection(vec![
                    Box::new(TermQuery::new(
                        Term::from_field_text(f.id, "hot"),
                        IndexRecordOption::Basic,
                    )),
                    Box::new(TermQuery::new(
                        Term::from_field_text(f.kind, KIND_META),
                        IndexRecordOption::Basic,
                    )),
                ])))
                .unwrap();
            let mut body = TantivyDocument::default();
            body.add_text(f.id, "hot");
            body.add_text(f.kind, KIND_BODY);
            body.add_text(f.body, format!("common appendword{n:08x}"));
            writer.add_document(body).unwrap();
            let mut meta = TantivyDocument::default();
            meta.add_text(f.id, "hot");
            meta.add_text(f.kind, KIND_META);
            writer.add_document(meta).unwrap();
            writer.commit().unwrap();
            let small = index
                .searchable_segment_metas()
                .unwrap()
                .into_iter()
                .find(|meta| !cold.contains(&meta.id()) && meta.num_deleted_docs() == 0)
                .unwrap();
            assert!(segment_bytes(&dir, &small) < floor);
            let term = format!("appendword{n:08x}");
            let before = session_ids_and_reads(&dir, "common");
            let cold_hit = session_ids_and_reads(&dir, "coldword00000000");
            let append_hit = session_ids_and_reads(&dir, &term);
            assert_eq!(cold_hit, (vec!["cold".into()], 1));
            assert_eq!(append_hit, (vec!["hot".into()], 1));
            let work = measured_compact(
                &dir,
                &index,
                &mut writer,
                if legacy { floor * TIER_FANOUT as u64 } else { floor },
                &mut cold,
            );
            total.inputs += work.inputs;
            total.outputs += work.outputs;
            total.merges += work.merges;
            total.cold_rewrites += work.cold_rewrites;
            assert_eq!(session_ids_and_reads(&dir, "common"), before);
            assert_eq!(session_ids_and_reads(&dir, "coldword00000000"), cold_hit);
            assert_eq!(session_ids_and_reads(&dir, &term), append_hit);
            assert_eq!(before.0, ["cold", "hot"]);
            assert_eq!(
                before.1,
                n + 2,
                "one identity request per live matching body, not every stored document"
            );
        }
        total.final_segments = index.searchable_segment_ids().unwrap().len();
        let (ids, reads) = session_ids_and_reads(&dir, "common");
        assert_eq!(
            measured_compact(
                &dir,
                &index,
                &mut writer,
                if legacy { floor * TIER_FANOUT as u64 } else { floor },
                &mut cold
            )
            .merges,
            0
        );
        std::fs::remove_dir_all(dir).ok();
        (total, ids, reads)
    }
    #[test]
    fn append_compaction_keeps_the_cold_segment_above_the_bottom_tier() {
        let (legacy, ids, reads) = append_merge_control(true);
        let (fixed, fixed_ids, fixed_reads) = append_merge_control(false);
        assert_eq!((ids, reads), (fixed_ids, fixed_reads));
        assert!(legacy.cold_rewrites >= 2, "legacy: {legacy:?}");
        assert_eq!(fixed.cold_rewrites, 0, "fixed: {fixed:?}");
        assert!(fixed.inputs < legacy.inputs, "inputs {fixed:?} versus {legacy:?}");
        assert!(fixed.outputs < legacy.outputs, "outputs {fixed:?} versus {legacy:?}");
        eprintln!("append merge bytes: legacy={legacy:?}, fixed={fixed:?}");
    }

    /// Opt-in operation control on a disposable copy of an existing index. The byte-tier
    /// policy is schema-independent: old indexes are useful geometry controls, but this does
    /// not exercise their schema migration or claim parity for a newly rebuilt fleet index.
    #[test]
    #[ignore = "requires a disposable index copied by tools/profiling/compaction-existing.py"]
    fn existing_index_compaction_work_control() {
        let dir = PathBuf::from(std::env::var_os("CV_COMPACTION_CONTROL_INDEX").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join("cv-compaction-control-copy")).unwrap(),
            "disposable clone\n"
        );
        let index = Index::open_in_dir(&dir).unwrap();
        let schema = index.schema();
        let id = schema.get_field("id").unwrap();
        let body = schema.get_field("body").unwrap();
        let queries = ["error", "test", "cache"];
        let query_ids = |term: &str| {
            let searcher = index.reader().unwrap().searcher();
            let query = QueryParser::for_index(&index, vec![body]).parse_query(term).unwrap();
            let addresses = searcher.search(&query, &tantivy::collector::DocSetCollector).unwrap();
            let mut ids: Vec<String> = addresses
                .into_iter()
                .map(|address| {
                    searcher
                        .doc::<TantivyDocument>(address)
                        .unwrap()
                        .get_first(id)
                        .and_then(|value| value.as_str())
                        .unwrap()
                        .to_owned()
                })
                .collect();
            ids.sort();
            ids
        };
        let before_queries: Vec<_> = queries.iter().map(|term| query_ids(term)).collect();
        let before = index.searchable_segment_metas().unwrap();
        let original_large: HashSet<_> = before
            .iter()
            .filter(|meta| segment_bytes(&dir, meta) > TIER_FLOOR)
            .map(|meta| meta.id())
            .collect();
        let initial_segments: Vec<_> = before
            .iter()
            .map(|meta| {
                serde_json::json!({
                    "bytes": segment_bytes(&dir, meta),
                    "documents": meta.max_doc(), "deleted_documents": meta.num_deleted_docs()
                })
            })
            .collect();
        let mut writer = open_writer(&index, 15_000_000).unwrap();
        let mut cold = original_large.clone();
        let mut steps = Vec::new();
        for n in 0..16 {
            let mut doc = TantivyDocument::default();
            doc.add_text(id, format!("cv-compaction-control-{n}"));
            doc.add_text(body, format!("zqcvcompactioncontrol{n:08}"));
            writer.add_document(doc).unwrap();
            writer.commit().unwrap();
            let work = measured_compact(&dir, &index, &mut writer, TIER_FLOOR, &mut cold);
            if work.merges > 0 {
                for (term, expected) in queries.iter().zip(&before_queries) {
                    assert_eq!(query_ids(term), *expected, "query multiset changed for {term}");
                }
            }
            for k in 0..=n {
                assert_eq!(
                    query_ids(&format!("zqcvcompactioncontrol{k:08}")),
                    [format!("cv-compaction-control-{k}")]
                );
            }
            steps.push(serde_json::json!({
                "append": n + 1, "merge_input_bytes": work.inputs,
                "merge_output_bytes": work.outputs, "merges": work.merges,
                "original_large_lineage_rewrites": work.cold_rewrites,
                "segments": index.searchable_segment_ids().unwrap().len()
            }));
        }
        assert!(compaction_candidates(&dir, &index).unwrap().is_none());
        let final_ids: HashSet<_> = index.searchable_segment_ids().unwrap().into_iter().collect();
        let report = serde_json::json!({
            "schema_current": schema_current(&schema), "production_tier_floor": TIER_FLOOR,
            "initial_segments": initial_segments, "steps": steps,
            "query_matching_document_counts": queries.into_iter().zip(before_queries.iter().map(Vec::len)).collect::<std::collections::BTreeMap<_, _>>(),
            "query_multisets_unchanged_after_each_merge": true,
            "all_synthetic_append_terms_retained": true,
            "original_large_segments_retained": original_large.is_subset(&final_ids),
            "final_segments": final_ids.len(), "unchanged_pending_compaction": false
        });
        let report_path = std::env::var_os("CV_COMPACTION_CONTROL_REPORT").unwrap();
        std::fs::write(report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }

    /// Regression for the live-staleness disclosure: a session that grows after indexing must be
    /// counted by [`staleness_over`], and a fresh index must report none — the footer `cv search`
    /// appends is only honest if both directions hold.
    #[test]
    fn staleness_counts_grown_sessions_then_goes_quiet() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "s1", "alpha corpus seed");
        let refs = vec![sref("s1", "one", p.clone())];

        index_refs(&dir, &refs, false).unwrap();
        // A fresh index over the same files reports no staleness — the footer stays off.
        assert_eq!(staleness_over(&dir, &refs).map(|s| s.stale_sessions), Some(0));
        assert_eq!(staleness_line(&staleness_over(&dir, &refs).unwrap()), None);

        // Grow the transcript on disk: its signature no longer matches its stamp → counted, and
        // the note names the count and the cure.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        use std::io::Write as _;
        writeln!(f, "{}", serde_json::json!({"type":"user","uuid":"u2","sessionId":"s1","message":{"role":"user","content":"beta appended"}})).unwrap();
        let s = staleness_over(&dir, &refs).expect("index present");
        assert_eq!(s.stale_sessions, 1, "the grown session must count as stale");
        assert!(s.behind_ns >= 0);
        let note = staleness_line(&s).expect("a stale session must produce the note");
        assert!(note.contains("behind for 1 session"), "{note}");
        assert!(note.contains("cv index"), "{note}");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// A session the index has never seen counts as stale too — a hit list that can't contain it
    /// must disclose the gap.
    #[test]
    fn staleness_counts_unindexed_sessions() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "old", "alpha already indexed");
        let p2 = write_claude(&sdir, "new", "beta never indexed");
        index_refs(&dir, &[sref("old", "one", p.clone())], false).unwrap();

        let refs = vec![sref("old", "one", p), sref("new", "two", p2)];
        assert_eq!(staleness_over(&dir, &refs).map(|s| s.stale_sessions), Some(1));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }
}
