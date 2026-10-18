//! # cv-search
//!
//! Pure-Rust search over clustervision sessions, in two flavours:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, `text_search` queries it and returns highlighted snippets. This is a
//!    strict upgrade over a SQLite FTS5 index: real tokenization, BM25 scoring, fielded queries
//!    (`harness:claude foo`), phrase/boolean operators, and fast incremental commits.
//!
//! 2. **Semantic** via [`model2vec-rs`] (distilled *static* embeddings — tiny, fast, no ONNX
//!    runtime). `embed_all` embeds every session and persists the vectors; `semantic_search`
//!    embeds the query and does a brute-force cosine ranking. For ~1k sessions an in-memory
//!    `Vec<(id, Vec<f32>)>` scan is plenty — no ANN crate needed. Gated behind the `semantic`
//!    cargo feature (default-on, since model2vec is lightweight); a minimal FTS-only build is
//!    `cargo build -p cv-search --no-default-features`.
//!
//! Both indexes live under `$CLUSTERVISION_HOME` (or `~/.clustervision`): the tantivy index in
//! `…/tantivy/` and the embedding store in `…/embeddings.bin`.

#[cfg(feature = "semantic")]
use anyhow::Context;
use anyhow::Result;
use std::path::PathBuf;

pub mod fts;
mod ingest;
#[cfg(feature = "semantic")]
pub mod semantic;
#[cfg(feature = "semantic")]
mod semantic_corpus;
mod session_collector;

/// A single search result, shared by full-text and semantic search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Relevance score (BM25 for FTS, cosine similarity for semantic).
    pub score: f32,
    /// A stored preview, or a live excerpt when a displayed full-text hit has been enriched.
    pub snippet: String,
    /// Unix timestamps carried straight off the FTS index / embedding store, so callers can date
    /// rows without re-discovering the corpus. `None` when the session itself had no parseable
    /// dates, or for semantic hits from an embedding store written before dates were recorded
    /// (re-run `embed_all` to backfill).
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Sub-agent provenance: when this hit is a folded-in sub-agent transcript (indexed with
    /// `cv index --subagents`), the top-level session that spawned it, the agent's own id, and the
    /// workflow run it belonged to (if any). All `None` for an ordinary top-level session, and for
    /// any hit from an index built before subagent folding (the fields default-absent on the
    /// stored doc and on deserialization of older serialized hits).
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    /// A live scan's hit is the discovered session itself; its row is exact, scoreless and never
    /// a sub-agent.
    #[serde(skip)]
    pub session: Option<cv_core::SessionRef>,
}

impl From<cv_core::scan::LiveHit> for Hit {
    fn from(hit: cv_core::scan::LiveHit) -> Self {
        let r = hit.session;
        Hit {
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            title: Some(hit.title),
            score: 0.0,
            snippet: hit.snippet,
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            parent_id: None,
            agent_id: None,
            workflow: None,
            session: Some(r),
        }
    }
}

/// The §3 search row for a hit, through one operation-owned catalog reader: the index's row with
/// its score and provenance, or the live scan's exact row ([`cv_core::rows::live_search_row`]).
pub fn row(hit: &Hit, context: &cv_core::rows::SearchRowContext) -> serde_json::Value {
    if let Some(session) = &hit.session {
        return cv_core::rows::live_search_row(session, hit.title.as_deref().unwrap_or_default(), &hit.snippet);
    }
    cv_core::rows::SearchRow {
        id: &hit.id,
        harness: &hit.harness,
        cwd: hit.cwd.as_deref(),
        title: hit.title.as_deref(),
        created_at: hit.created_at,
        updated_at: hit.updated_at,
        score: Some(hit.score),
        snippet: &hit.snippet,
        agent_id: hit.agent_id.as_deref(),
        parent_id: hit.parent_id.as_deref(),
        workflow: hit.workflow.as_deref(),
    }
    .to_json_with(context)
}

/// Which path answers a search: the index (authoritative; an empty result is "no match"), the
/// semantic vectors, or a head-capped live scan of every transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Index,
    Semantic,
    Live,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Index => "index",
            Source::Semantic => "semantic",
            Source::Live => "live",
        }
    }
}

/// Why an answer may be partial. Every door states these in its own words; none is an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// How far the index trails the corpus right now ([`fts::staleness_note`]).
    Stale(String),
    /// Nothing matched, and the index holds no sub-agent transcripts (`cv index --subagents`).
    NoSubagents,
    /// Live snippets could not be generated for the shown hits; the indexed previews stand in.
    SnippetsUnavailable(String),
}

/// What one search found: the hits in rank order (or discovery order, live), where they came
/// from, the degradations, and whether a live scan stopped at the limit.
pub struct Found {
    pub hits: Vec<Hit>,
    pub source: Source,
    pub notes: Vec<Note>,
    pub truncated: bool,
}

/// The one search operation every door renders: `cv search` and `GET /api/search` call this with
/// the same filter and get the same hits, source and notes; a long-lived service keeps one so the
/// semantic model and store stay resident.
#[derive(Default)]
pub struct Searcher {
    #[cfg(feature = "semantic")]
    pub semantic: SemanticSearch,
}

impl Searcher {
    /// Errors are the path's own (an unusable index, an invalid query, a missing model); the
    /// caller adds the door's pointer. Filters apply before ranking and once more after it.
    pub fn search(&self, query: &str, filter: SearchFilter<'_>, limit: usize, source: Source) -> Result<Found> {
        let mut found = Found {
            hits: Vec::new(),
            source,
            notes: Vec::new(),
            truncated: false,
        };
        let dir = default_tantivy_dir();
        match source {
            Source::Live => {
                let want = filter.harness.and_then(cv_core::Harness::parse);
                let live = cv_core::scan::live_search_filtered(query, want, limit, |r| {
                    filter.cwd.is_none_or(|needle| {
                        r.cwd
                            .as_ref()
                            .is_some_and(|path| path.to_string_lossy().contains(needle))
                    })
                })?;
                found.hits = live.hits.into_iter().map(Hit::from).collect();
                found.truncated = live.truncated;
                return Ok(found);
            }
            Source::Index => found.hits = fts::text_search_indexed_filtered(&dir, query, limit, filter)?,
            #[cfg(feature = "semantic")]
            Source::Semantic => found.hits = self.semantic.search(query, limit, filter)?,
            #[cfg(not(feature = "semantic"))]
            Source::Semantic => anyhow::bail!("semantic search is not built into this cv"),
        }
        found.hits.retain(|h| {
            filter.harness.is_none_or(|w| h.harness == w)
                && filter
                    .cwd
                    .is_none_or(|needle| h.cwd.as_ref().is_some_and(|c| c.contains(needle)))
        });
        found.hits.truncate(limit);
        if source == Source::Index {
            if let Err(e) = fts::enrich_text_snippets(&dir, query, filter.scope, &mut found.hits) {
                found.notes.push(Note::SnippetsUnavailable(format!("{e:#}")));
            }
            // Named on hits and misses: a hit list can be missing rows, not only an empty one.
            if let Some(stale) = fts::staleness_note(&dir) {
                found.notes.push(Note::Stale(stale));
            }
            if found.hits.is_empty() && !fts::has_subagent_docs(&dir) {
                found.notes.push(Note::NoSubagents);
            }
        }
        Ok(found)
    }
}

/// Root directory for cv-search's on-disk state: `$CLUSTERVISION_HOME` or `~/.clustervision`.
///
/// Back-compat: the project was once "claurdvoyant", so we still honor `$CLAURDVOYANT_HOME` and an
/// existing `~/.claurdvoyant` (kept only if the new dir hasn't been created) — so a previously-built
/// index/embedding store isn't orphaned by the rename.
pub fn home_dir() -> PathBuf {
    if let Some(v) = cv_core::env::var_os("CLUSTERVISION_HOME").or_else(|| cv_core::env::var_os("CLAURDVOYANT_HOME")) {
        return PathBuf::from(v);
    }
    let Some(home) = cv_core::env::home_dir() else {
        return PathBuf::from(".");
    };
    let new = home.join(".clustervision");
    let legacy = home.join(".claurdvoyant");
    if !new.exists() && legacy.exists() {
        legacy
    } else {
        new
    }
}

/// Default location of the tantivy full-text index.
pub fn default_tantivy_dir() -> PathBuf {
    home_dir().join("tantivy")
}

/// Default location of the semantic embedding store (binary; see `semantic` module docs).
pub fn default_embeddings_path() -> PathBuf {
    home_dir().join("embeddings.bin")
}

// ---- Convenience wrappers over the default-located indexes ---------------------------------

/// Discover, parse, and update the full-text index at its default location. Incremental by default
/// (only changed/new sessions are rewritten; vanished ones are reaped); `rebuild = true` clears and
/// rebuilds from scratch. Returns what it indexed (the sessions it could not read included; `strict`
/// makes any of them an error), and the discovered refs. Each compressed
/// rollout is decoded once: discovery hands its decoded bytes to the sweep running beside it.
pub fn index_all(
    dir: impl Into<Option<PathBuf>>,
    rebuild: bool,
    subagents: bool,
    strict: bool,
) -> Result<(fts::Indexed, Vec<cv_core::SessionRef>)> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::index_all(&dir, rebuild, subagents, strict)
}

/// Update the index using an already discovered session inventory.
pub fn index_sessions(
    dir: impl Into<Option<PathBuf>>,
    refs: impl AsRef<[cv_core::SessionRef]>,
    rebuild: bool,
    subagents: bool,
) -> Result<usize> {
    fts::index_sessions(
        &dir.into().unwrap_or_else(default_tantivy_dir),
        refs,
        rebuild,
        subagents,
    )
}

/// Run a full-text query against the index at its default location.
pub fn text_search(dir: impl Into<Option<PathBuf>>, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::text_search(&dir, query, limit)
}

/// Distinguish an invalid full-text expression from an unavailable index. Callers may
/// degrade storage/setup failures to live search, but a rejected query is a caller error.
pub fn is_invalid_text_query(error: &anyhow::Error) -> bool {
    error.is::<tantivy::query::QueryParserError>()
}

/// Match every indexed session ID, without ranking or reading source transcripts.
pub fn text_search_ids(dir: impl Into<Option<PathBuf>>, query: &str) -> Result<std::collections::HashSet<String>> {
    fts::text_search_ids(&dir.into().unwrap_or_else(default_tantivy_dir), query)
}

/// Match several full-text needles using one index-reader snapshot. The outer result reports
/// index setup errors; each returned result independently reports that needle's query error.
pub fn text_search_ids_batch(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
) -> Result<Vec<Result<std::collections::HashSet<String>>>> {
    fts::text_search_ids_batch(&dir.into().unwrap_or_else(default_tantivy_dir), queries)
}

/// Rank sessions with stored previews; no source transcripts are read.
pub fn text_search_indexed(dir: impl Into<Option<PathBuf>>, query: &str, limit: usize) -> Result<Vec<Hit>> {
    fts::text_search_indexed(&dir.into().unwrap_or_else(default_tantivy_dir), query, limit)
}

/// Filters evaluated before ranking sessions. Cwd matching is a case-sensitive substring.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchFilter<'a> {
    pub harness: Option<&'a str>,
    pub cwd: Option<&'a str>,
    /// Which text a full-text query matches; semantic vectors embed the exchange alone.
    pub scope: Scope,
}

/// Which of a session's text a full-text query matches. A session's own exchange by default:
/// what the person asked, what the model replied and ran, what came back
/// ([`cv_core::MessageKind::is_exchange`]). The context around it (injected instruction files,
/// system prompts, skill bodies, hook output, notices, compaction summaries) is the same in many
/// sessions and would dilute every answer, so it is matched only when asked for, and turns a fork
/// copied from its source are matched in the source alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Exchange,
    Context,
    All,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "exchange" => Some(Scope::Exchange),
            "context" => Some(Scope::Context),
            "all" => Some(Scope::All),
            _ => None,
        }
    }
    /// Whether a turn of `kind` is in this scope.
    pub fn holds(self, kind: cv_core::MessageKind) -> bool {
        match self {
            Scope::Exchange => kind.is_exchange(),
            Scope::Context => !kind.is_exchange(),
            Scope::All => true,
        }
    }
}

/// Rank the requested number of matching sessions, applying filters before the ranking cutoff.
pub fn text_search_indexed_filtered(
    dir: impl Into<Option<PathBuf>>,
    query: &str,
    limit: usize,
    filter: SearchFilter<'_>,
) -> Result<Vec<Hit>> {
    fts::text_search_indexed_filtered(&dir.into().unwrap_or_else(default_tantivy_dir), query, limit, filter)
}

/// Generate live snippets for only the selected displayed hits.
pub fn enrich_text_snippets(
    dir: impl Into<Option<PathBuf>>,
    query: &str,
    scope: Scope,
    hits: &mut [Hit],
) -> Result<()> {
    fts::enrich_text_snippets(&dir.into().unwrap_or_else(default_tantivy_dir), query, scope, hits)
}

/// Discover, parse, and embed every session, persisting vectors to the default store.
#[cfg(feature = "semantic")]
pub fn embed_all(path: impl Into<Option<PathBuf>>) -> Result<usize> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::embed_all(&path)
}

/// Refresh semantic vectors using the same inventory as the full-text pass.
#[cfg(feature = "semantic")]
pub fn embed_sessions(path: impl Into<Option<PathBuf>>, refs: &[cv_core::SessionRef]) -> Result<usize> {
    semantic::embed_sessions(&path.into().unwrap_or_else(default_embeddings_path), refs)
}

pub use fts::{Indexed, Unreadable};
/// Explicit lazy semantic state for a long-lived service. One-shot search remains stateless.
#[cfg(feature = "semantic")]
pub use semantic::SemanticSearch;

/// Embed the query and rank stored session vectors by cosine similarity (default store).
#[cfg(feature = "semantic")]
pub fn semantic_search(path: impl Into<Option<PathBuf>>, query: &str, k: usize) -> Result<Vec<Hit>> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::semantic_search(&path, query, k)
}

/// Apply session metadata filters before the semantic ranking cutoff.
#[cfg(feature = "semantic")]
pub fn semantic_search_filtered(
    path: impl Into<Option<PathBuf>>,
    query: &str,
    k: usize,
    filter: SearchFilter<'_>,
) -> Result<Vec<Hit>> {
    semantic::semantic_search_filtered(&path.into().unwrap_or_else(default_embeddings_path), query, k, filter)
}

/// Read only the semantic projection demanded for one discovered session.
#[cfg(feature = "semantic")]
pub(crate) fn read_corpus_doc(r: &cv_core::SessionRef) -> Result<(Doc, cv_core::source::ReadSet)> {
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    let mut sink = semantic_corpus::DocSink::new(cv_core::Resolver::new(Some(r.path.clone())));
    sink.copied_before = adapter.copied_before(r, &mut sink);
    let meta = cv_core::stream::stream_counted(
        &*adapter,
        r,
        &cv_core::ParseOptions::lazy(),
        &mut sink,
        cv_core::stream::Pass::RECORD,
    )
    .with_context(|| format!("reading semantic corpus session {} ({})", r.id, r.harness))?
    .session;
    let title = cv_core::label_from(meta.title.as_deref(), sink.first_prompt.as_deref());
    Ok((
        Doc {
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            title: Some(title),
            body: sink.body.into_string(),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
        },
        sink.reads,
    ))
}

/// One indexable document distilled from a parsed [`cv_core::Session`], for the **semantic** path
/// ([`semantic::embed_all`]), which reads exactly these fields. The FTS path no longer goes through
/// `Doc`: it streams straight into `fts::ChunkSink`, which reads dates/path/mtime off the
/// [`cv_core::SessionRef`] instead (so they aren't duplicated here).
#[cfg(feature = "semantic")]
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub body: String,
    /// Unix-seconds session dates off the discovery [`cv_core::SessionRef`], so semantic hits
    /// carry `created_at`/`updated_at` just like FTS hits do.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

#[cfg(all(test, feature = "probe-heap"))]
#[global_allocator]
static PROBE_ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
