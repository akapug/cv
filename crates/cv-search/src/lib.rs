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
/// A legacy `embeddings.json` next to it is migrated on first query.
pub fn default_embeddings_path() -> PathBuf {
    home_dir().join("embeddings.bin")
}

// ---- Convenience wrappers over the default-located indexes ---------------------------------

/// Discover, parse, and update the full-text index at its default location. Incremental by default
/// (only changed/new sessions are rewritten; vanished ones are reaped); `rebuild = true` clears and
/// rebuilds from scratch. Returns the number of sessions discovered, and their refs. Each compressed
/// rollout is decoded once: discovery hands its decoded bytes to the sweep running beside it.
pub fn index_all(
    dir: impl Into<Option<PathBuf>>,
    rebuild: bool,
    subagents: bool,
) -> Result<(usize, Vec<cv_core::SessionRef>)> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::index_all(&dir, rebuild, subagents)
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
pub fn enrich_text_snippets(dir: impl Into<Option<PathBuf>>, query: &str, hits: &mut [Hit]) -> Result<()> {
    fts::enrich_text_snippets(&dir.into().unwrap_or_else(default_tantivy_dir), query, hits)
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
    let meta = cv_core::stream::stream_counted(
        &*adapter,
        r,
        &cv_core::ParseOptions::lazy(),
        &mut sink,
        cv_core::stream::Pass::RECORD,
    )
    .with_context(|| format!("reading semantic corpus session {} ({})", r.id, r.harness))?
    .session;
    let title = cv_core::label_from(meta.title.as_deref(), sink.first_user_text.as_deref());
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
