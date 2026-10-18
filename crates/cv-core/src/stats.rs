//! Corpus-wide statistics over a set of discovered sessions — what `cv stats` prints and what
//! `cv serve`'s `/api/stats` serves.
//!
//! The numbers come from the catalog row alone (harness, message count, cwd, dates), so a whole
//! fleet costs one catalog read and no transcript parsing. Both doors call [`CorpusStats::compute`]
//! and [`CorpusStats::to_json`] so the dashboard's Stats view and `cv stats --json` cannot drift —
//! the dashboard used to accumulate its own totals from whichever sessions the user had clicked,
//! and said so in the UI.

use crate::ir::{home_rel, Harness, Message, SessionRef, Usage};
use crate::spill::{ExactStrings, Ordered, Record};
use crate::stream::{Flow, ParseOptions};
#[cfg(feature = "sqlite")]
mod usage_cache;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

/// How many cwds `to_json` (and `cv stats`' table) keeps.
pub const TOP_CWDS: usize = 10;

/// The aggregate. `by_harness` and `top_cwds` are sorted by count descending, ties broken by name
/// ascending, so the order is stable across runs and across doors.
#[derive(Debug, Clone, Default)]
pub struct CorpusStats {
    pub sessions: usize,
    pub messages: usize,
    pub by_harness: Vec<(&'static str, usize)>,
    /// Every cwd, home-relative (`~/dev/x`), `(no cwd)` for sessions that record none. The full
    /// list; [`CorpusStats::to_json`] and the table take the first [`TOP_CWDS`].
    pub top_cwds: Vec<(String, usize)>,
    pub earliest_created: Option<DateTime<Utc>>,
    pub latest_updated: Option<DateTime<Utc>>,
}

impl CorpusStats {
    pub fn compute(refs: &[SessionRef]) -> CorpusStats {
        let mut per_harness: HashMap<&'static str, usize> = HashMap::new();
        let mut per_cwd: HashMap<String, usize> = HashMap::new();
        let mut stats = CorpusStats {
            sessions: refs.len(),
            ..CorpusStats::default()
        };
        for r in refs {
            *per_harness.entry(r.harness.as_str()).or_default() += 1;
            stats.messages += r.message_count;
            let cwd = r.cwd.as_deref().map(home_rel).unwrap_or_else(|| "(no cwd)".into());
            *per_cwd.entry(cwd).or_default() += 1;
            if let Some(c) = r.created_at {
                stats.earliest_created = Some(stats.earliest_created.map_or(c, |m| m.min(c)));
            }
            if let Some(u) = r.updated_at {
                stats.latest_updated = Some(stats.latest_updated.map_or(u, |m| m.max(u)));
            }
        }
        stats.by_harness = per_harness.into_iter().collect();
        stats.by_harness.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        stats.top_cwds = per_cwd.into_iter().collect();
        stats.top_cwds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        stats
    }

    /// The `cv stats --json` payload, whose top-level keys are exactly `sessions, messages,
    /// by_harness, top_cwds, earliest_created, latest_updated`. `by_harness` is an object in
    /// descending-count order (the workspace's serde_json keeps insertion order).
    pub fn to_json(&self) -> Value {
        let by_harness: serde_json::Map<String, Value> =
            self.by_harness.iter().map(|(h, n)| (h.to_string(), json!(n))).collect();
        let top_cwds: Vec<Value> = self
            .top_cwds
            .iter()
            .take(TOP_CWDS)
            .map(|(c, n)| json!({ "cwd": c, "sessions": n }))
            .collect();
        json!({
            "sessions": self.sessions,
            "messages": self.messages,
            "by_harness": by_harness,
            "top_cwds": top_cwds,
            "earliest_created": self.earliest_created.map(|d| d.to_rfc3339()),
            "latest_updated": self.latest_updated.map(|d| d.to_rfc3339()),
        })
    }
}

/// Token usage rolled up per `(harness, model)`: what `cv stats --tokens` prints.
///
/// Unlike [`CorpusStats`] this parses every transcript (usage lives on assistant messages, not in
/// the catalog), and it widens each Claude session to its sub-agent forest — direct and `Workflow`
/// sub-agents are separate transcripts the catalog does not list, and on an orchestrating session
/// they hold most of the spend. Counts follow [`Usage`]'s disjoint convention, so rows from
/// different harnesses add up.
#[derive(Debug, Clone, Default)]
pub struct TokenStats {
    /// Sorted by [`TokenRow::total`] descending, ties by harness then model.
    pub rows: Vec<TokenRow>,
    /// Catalog sessions parsed.
    pub sessions: usize,
    /// Sub-agent transcripts parsed on top of them.
    pub subagents: usize,
    /// Transcripts that failed to parse (counted, not fatal: one bad file shouldn't hide a fleet).
    pub failed: usize,
    /// Messages whose usage was dropped as a repeat of one already counted (see [`usage_key`]).
    pub duplicates: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenRow {
    pub harness: &'static str,
    /// The message's model, else the session's, else `(unknown)`.
    pub model: String,
    /// Assistant messages that carried usage.
    pub calls: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// Separately reported subset of `output`; missing reasoning usage contributes zero.
    /// Zero therefore does not establish absence of reasoning (Claude reports no separate field).
    pub reasoning: u64,
    /// Summed provider-reported cost; `None` when no message in the row reported one.
    pub cost_usd: Option<f64>,
}

impl TokenRow {
    /// Everything the model read or wrote: input + both cache counts + output.
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    /// The part that was not a cache read: input + cache writes + output.
    pub fn uncached(&self) -> u64 {
        self.input + self.cache_write + self.output
    }

    fn add(&mut self, u: &Usage) {
        self.calls += 1;
        self.input += u.input_tokens.unwrap_or(0);
        self.cache_read += u.cache_read_tokens.unwrap_or(0);
        self.cache_write += u.cache_creation_tokens.unwrap_or(0);
        self.output += u.output_tokens.unwrap_or(0);
        self.reasoning += u.reasoning_tokens.unwrap_or(0);
        if let Some(c) = u.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
        }
    }

    fn absorb(&mut self, o: &TokenRow) {
        self.calls += o.calls;
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.reasoning += o.reasoning;
        if let Some(c) = o.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "harness": self.harness,
            "model": self.model,
            "calls": self.calls,
            "input": self.input,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "output": self.output,
            "reasoning": self.reasoning,
            "uncached": self.uncached(),
            "total": self.total(),
            "cost_usd": self.cost_usd,
        })
    }
}

/// One usage-bearing message, as a transcript's parse hands it to the fold.
struct UsageHit {
    key: Option<String>,
    harness: Harness,
    model: String,
    usage: Usage,
}

/// The identity that makes two messages *the same API call*, when the harness records one.
///
/// Claude Code writes one JSONL line per content block of a streamed response, each repeating the
/// response's `usage`, and a resumed session copies its predecessor's history into the new file —
/// so the same `message.id` recurs within and across transcripts. Other harnesses record usage once
/// per call and carry no such id, so they are never deduplicated.
pub(crate) fn usage_key(h: Harness, m: &Message) -> Option<String> {
    match h {
        Harness::Claude => m
            .harness_extra(Harness::Claude)
            .and_then(|b| b.get("message.id"))
            .and_then(Value::as_str)
            .map(|id| format!("claude:{id}")),
        _ => None,
    }
}

/// A transcript is staged until EOF: failures discard every hit and missing per-message models
/// use the final session model. The bounded spool preserves record and floating-point order.
struct UsageBatch {
    hits: UsageRecords,
    fallback_model: String,
    #[cfg(feature = "sqlite")]
    publication: Option<(SessionRef, crate::source::Baseline)>,
}
enum UsageRecords {
    Staged(Ordered<UsageHit>),
    #[cfg(feature = "sqlite")]
    Cached(usage_cache::CachedRows),
}
impl UsageRecords {
    fn rows(self) -> Result<Box<dyn Iterator<Item = Result<UsageHit>>>> {
        Ok(match self {
            Self::Staged(hits) => Box::new(hits.rows()?),
            #[cfg(feature = "sqlite")]
            Self::Cached(rows) => Box::new(rows),
        })
    }
}

fn usage_hits(
    r: &SessionRef,
    #[cfg(feature = "sqlite")] cache: Option<&usage_cache::Cache>,
) -> Result<Option<UsageBatch>> {
    let Some(adapter) = crate::harness::for_harness(r.harness) else {
        return Ok(None);
    };
    #[cfg(feature = "sqlite")]
    // Tiny native transcripts cost less to parse than to validate and replay a ledger.
    let cache = cache.filter(|_| {
        !matches!(r.harness, Harness::Claude | Harness::Codex)
            || std::fs::metadata(adapter.resume_source(r)).is_ok_and(|metadata| metadata.len() >= 64 * 1024)
    });
    #[cfg(feature = "sqlite")]
    let source = cache.map(|_| crate::source::Probe::default().snapshot(r));
    #[cfg(feature = "sqlite")]
    if let Some(batch) = cache.and_then(|cache| cache.load(r, source.as_ref()?)) {
        return Ok(Some(batch));
    }
    struct Sink {
        hits: Ordered<UsageHit>,
        harness: Harness,
        error: Option<anyhow::Error>,
        #[cfg(feature = "sqlite")]
        reads: Option<crate::source::ReadSet>,
    }
    impl crate::MessageSink for Sink {
        fn message(&mut self, m: Message) -> Flow {
            if let Some(usage) = m
                .usage
                .as_ref()
                .filter(|u| u.total_tokens() > 0 || u.cost_usd.is_some())
            {
                if let Err(error) = self.hits.push(UsageHit {
                    key: usage_key(self.harness, &m),
                    harness: self.harness,
                    model: m.model.clone().unwrap_or_default(),
                    usage: usage.clone(),
                }) {
                    self.error = Some(error);
                    return Flow::Stop;
                }
            }
            Flow::Continue
        }
        #[cfg(feature = "sqlite")]
        fn dependency(&mut self, path: &std::path::Path) {
            if let Some(reads) = &mut self.reads {
                reads.record(path);
            }
        }
    }
    let mut sink = Sink {
        hits: Ordered::default(),
        harness: r.harness,
        error: None,
        #[cfg(feature = "sqlite")]
        reads: source.as_ref().map(|_| Default::default()),
    };
    #[cfg(feature = "sqlite")]
    if let Some(reads) = &mut sink.reads {
        reads.record(&adapter.resume_source(r));
    }
    // Usage identity is selected without materializing unrelated rich sidecars.
    let session = adapter.try_stream(r, &ParseOptions::usage(), &mut sink);
    if let Some(error) = sink.error.take() {
        return Err(error);
    }
    let Ok(session) = session else {
        return Ok(None);
    };
    Ok(Some(UsageBatch {
        hits: UsageRecords::Staged(sink.hits),
        fallback_model: session.model.unwrap_or_else(|| "(unknown)".into()),
        #[cfg(feature = "sqlite")]
        publication: source
            .zip(sink.reads)
            .map(|(source, reads)| (r.clone(), source.with_reads(&reads))),
    }))
}

/// Binary usage rows keep cost bits exactly; serializing floats through a JSON staging codec
/// would add a second conversion and could change rounding before the ordered sum.
impl Record for UsageHit {
    type ReadState = Option<usize>;
    fn heap_bytes(&self) -> usize {
        self.key.as_ref().map_or(0, String::capacity) + self.model.capacity()
    }
    fn write(&self, writer: &mut impl Write) -> Result<()> {
        write_string(writer, self.key.as_deref())?;
        write_string(writer, Some(self.harness.as_str()))?;
        write_string(writer, Some(&self.model))?;
        for value in [
            self.usage.input_tokens,
            self.usage.output_tokens,
            self.usage.cache_read_tokens,
            self.usage.cache_creation_tokens,
            self.usage.reasoning_tokens,
            self.usage.cost_usd.map(f64::to_bits),
        ] {
            writer.write_all(&[u8::from(value.is_some())])?;
            if let Some(value) = value {
                writer.write_all(&value.to_le_bytes())?;
            }
        }
        Ok(())
    }
    fn read(reader: &mut impl BufRead, limit: &mut Option<usize>) -> Result<Self> {
        let key = read_string(reader, *limit)?;
        let harness = read_string(reader, *limit)?
            .and_then(|s| Harness::parse(&s))
            .context("invalid usage spool harness")?;
        let model = read_string(reader, *limit)?.context("missing usage spool model")?;
        let mut fields = [None; 6];
        for field in &mut fields {
            let mut flag = [0];
            reader.read_exact(&mut flag)?;
            *field = match flag[0] {
                0 => None,
                1 => {
                    let mut bytes = [0; 8];
                    reader.read_exact(&mut bytes)?;
                    Some(u64::from_le_bytes(bytes))
                }
                _ => anyhow::bail!("invalid usage spool field"),
            };
        }
        Ok(Self {
            key,
            harness,
            model,
            usage: Usage {
                input_tokens: fields[0],
                output_tokens: fields[1],
                cache_read_tokens: fields[2],
                cache_creation_tokens: fields[3],
                reasoning_tokens: fields[4],
                cost_usd: fields[5].map(f64::from_bits),
            },
        })
    }
}
fn write_string(writer: &mut impl Write, value: Option<&str>) -> Result<()> {
    writer.write_all(&[u8::from(value.is_some())])?;
    if let Some(value) = value {
        writer.write_all(&(value.len() as u64).to_le_bytes())?;
        writer.write_all(value.as_bytes())?;
    }
    Ok(())
}
fn read_string(reader: &mut impl BufRead, limit: Option<usize>) -> Result<Option<String>> {
    let mut flag = [0];
    reader.read_exact(&mut flag)?;
    match flag[0] {
        0 => Ok(None),
        1 => {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            let len = usize::try_from(u64::from_le_bytes(bytes)).context("usage spool string too large")?;
            anyhow::ensure!(
                limit.is_none_or(|limit| len <= limit),
                "usage cache string exceeds source budget"
            );
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes)?;
            Ok(Some(String::from_utf8(bytes).context("invalid usage spool UTF-8")?))
        }
        _ => anyhow::bail!("invalid usage spool string"),
    }
}

fn subagent_usage_refs(r: &SessionRef) -> Vec<SessionRef> {
    match r.harness {
        Harness::Claude => crate::harness::claude::subagent_session_refs(&r.path),
        _ => crate::subagent_tree_of(r).into_iter().map(|s| s.session).collect(),
    }
}

fn parse_workers() -> usize {
    #[cfg(feature = "parallel")]
    {
        rayon::current_num_threads().max(1)
    }
    #[cfg(not(feature = "parallel"))]
    {
        1
    }
}

fn parse_chunk(
    refs: &[SessionRef],
    #[cfg(feature = "sqlite")] cache: Option<&usage_cache::Cache>,
) -> Vec<Result<Option<UsageBatch>>> {
    let inventory = crate::source::inventory_context();
    let env = crate::env::Env::scoped();
    let parse = |r: &SessionRef| {
        let load = || {
            usage_hits(
                r,
                #[cfg(feature = "sqlite")]
                cache,
            )
        };
        crate::env::within(&env, || match &inventory {
            Some(context) => context.run(load),
            None => load(),
        })
    };
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        // Indexed collection preserves input order even when workers finish out of order.
        refs.par_iter().map(parse).collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        refs.iter().map(parse).collect()
    }
}

impl TokenStats {
    /// Ordinary source parse failures are counted; temporary storage failures are returned.
    pub fn compute(refs: &[SessionRef]) -> Result<TokenStats> {
        let _sources = crate::source::read_scope();
        let result = Self::compute_pass(refs, true);
        #[cfg(feature = "sqlite")]
        if result
            .as_ref()
            .is_err_and(|error| error.downcast_ref::<usage_cache::ReadError>().is_some())
        {
            // No totals leave a failed pass. Corrupt cached bytes are evicted and a complete
            // source pass recovers the result; the ordered exact-ID fold starts afresh.
            return Self::compute_pass(refs, false);
        }
        result
    }
    fn compute_pass(refs: &[SessionRef], _reuse: bool) -> Result<TokenStats> {
        #[cfg(feature = "sqlite")]
        let cache = usage_cache::Cache::new();
        let mut stats = TokenStats {
            sessions: refs.len(),
            ..TokenStats::default()
        };
        let subs: Vec<SessionRef> = crate::par_flat_map(refs.to_vec(), |r| subagent_usage_refs(&r));
        stats.subagents = subs.len();
        let mut fold = TokenFold::default();
        // Bound completed spool retention to one 256KiB allowance per configured worker.
        // Main sessions precede every subagent, matching the existing first-response-ID oracle.
        for group in [refs, subs.as_slice()] {
            for chunk in group.chunks(parse_workers()) {
                let parsed = parse_chunk(
                    chunk,
                    #[cfg(feature = "sqlite")]
                    if _reuse { cache.as_ref() } else { None },
                );
                for batch in parsed {
                    match batch? {
                        None => stats.failed += 1,
                        Some(batch) => {
                            #[cfg(feature = "sqlite")]
                            let mut publisher = batch.publication.and_then(|(r, source)| {
                                cache
                                    .as_ref()
                                    .and_then(|cache| cache.publisher(r, source, &batch.fallback_model))
                            });
                            for hit in batch.hits.rows()? {
                                let mut hit = hit?;
                                #[cfg(feature = "sqlite")]
                                if let Some(publisher) = &mut publisher {
                                    publisher.hit(&hit);
                                }
                                if hit.model.is_empty() {
                                    hit.model.clone_from(&batch.fallback_model);
                                }
                                fold.add(hit)?;
                            }
                            #[cfg(feature = "sqlite")]
                            if let Some(publisher) = publisher {
                                publisher.finish();
                            }
                        }
                    }
                }
                #[cfg(feature = "sqlite")]
                if let Some(cache) = &cache {
                    cache.finish_chunk();
                }
            }
        }
        #[cfg(feature = "sqlite")]
        if let Some(cache) = &cache {
            cache.finish();
        }
        fold.finish(&mut stats);
        Ok(stats)
    }

    #[cfg(test)]
    fn fold(&mut self, hits: impl Iterator<Item = UsageHit>) -> Result<()> {
        let mut fold = TokenFold::default();
        for hit in hits {
            fold.add(hit)?;
        }
        fold.finish(self);
        Ok(())
    }

    /// Per-harness subtotals (model = `*`), in the rows' ranking order by total.
    pub fn by_harness(&self) -> Vec<TokenRow> {
        let mut per: HashMap<&'static str, TokenRow> = HashMap::new();
        for r in &self.rows {
            per.entry(r.harness)
                .or_insert_with(|| TokenRow {
                    harness: r.harness,
                    model: "*".into(),
                    ..TokenRow::default()
                })
                .absorb(r);
        }
        let mut v: Vec<TokenRow> = per.into_values().collect();
        v.sort_by(|a, b| b.total().cmp(&a.total()).then(a.harness.cmp(b.harness)));
        v
    }

    /// The grand total (harness and model = `*`).
    pub fn total(&self) -> TokenRow {
        let mut t = TokenRow {
            harness: "*",
            model: "*".into(),
            ..TokenRow::default()
        };
        for r in &self.rows {
            t.absorb(r);
        }
        t
    }

    /// The `tokens` object `cv stats --tokens --json` adds: `{sessions, subagents, failed,
    /// duplicates, total, by_harness[], by_model[]}`.
    pub fn to_json(&self) -> Value {
        json!({
            "sessions": self.sessions,
            "subagents": self.subagents,
            "failed": self.failed,
            "duplicates": self.duplicates,
            "total": self.total().to_json(),
            "by_harness": self.by_harness().iter().map(TokenRow::to_json).collect::<Vec<_>>(),
            "by_model": self.rows.iter().map(TokenRow::to_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Default)]
struct TokenFold {
    seen: ExactStrings,
    rows: HashMap<(&'static str, String), TokenRow>,
    duplicates: usize,
}
impl TokenFold {
    fn add(&mut self, hit: UsageHit) -> Result<()> {
        if let Some(key) = hit.key {
            if !self.seen.insert(key)? {
                self.duplicates += 1;
                return Ok(());
            }
        }
        let harness = hit.harness.as_str();
        self.rows
            .entry((harness, hit.model.clone()))
            .or_insert_with(|| TokenRow {
                harness,
                model: hit.model,
                ..TokenRow::default()
            })
            .add(&hit.usage);
        Ok(())
    }
    fn finish(self, stats: &mut TokenStats) {
        stats.duplicates += self.duplicates;
        stats.rows = self.rows.into_values().collect();
        stats.rows.sort_by(|a, b| {
            b.total()
                .cmp(&a.total())
                .then(a.harness.cmp(b.harness))
                .then(a.model.cmp(&b.model))
        });
    }
}

/// The keys `cv stats --json` / `GET /api/stats` carry, in order.
#[cfg(test)]
pub const STATS_KEYS: [&str; 6] = [
    "sessions",
    "messages",
    "by_harness",
    "top_cwds",
    "earliest_created",
    "latest_updated",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;

    fn r(id: &str, h: Harness, msgs: usize, cwd: Option<&str>, created: &str, updated: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: h,
            path: format!("/x/{id}.jsonl").into(),
            cwd: cwd.map(Into::into),
            title: None,
            created_at: DateTime::parse_from_rfc3339(created)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            updated_at: DateTime::parse_from_rfc3339(updated)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            message_count: msgs,
        }
    }

    #[test]
    fn totals_ranking_and_json_keys() {
        let refs = vec![
            r(
                "a",
                Harness::Claude,
                3,
                Some("/w/one"),
                "2026-01-01T00:00:00Z",
                "2026-01-02T00:00:00Z",
            ),
            r(
                "b",
                Harness::Codex,
                5,
                Some("/w/one"),
                "2025-06-01T00:00:00Z",
                "2026-03-01T00:00:00Z",
            ),
            r(
                "c",
                Harness::Codex,
                7,
                None,
                "2026-02-01T00:00:00Z",
                "2026-02-02T00:00:00Z",
            ),
        ];
        let s = CorpusStats::compute(&refs);
        assert_eq!((s.sessions, s.messages), (3, 15));
        // Descending count, ties by name.
        assert_eq!(s.by_harness, vec![("codex", 2), ("claude", 1)]);
        assert_eq!(s.top_cwds[0], ("/w/one".to_string(), 2));
        assert!(s.top_cwds.iter().any(|(c, n)| c == "(no cwd)" && *n == 1));

        let v = s.to_json();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, STATS_KEYS, "stats payload keys, in order");
        let harnesses: Vec<&str> = v["by_harness"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(harnesses, ["codex", "claude"], "by_harness stays rank-ordered");
        assert_eq!(v["earliest_created"], "2025-06-01T00:00:00+00:00");
        assert_eq!(v["latest_updated"], "2026-03-01T00:00:00+00:00");
    }

    fn usage(input: u64, read: u64, write: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            cache_read_tokens: Some(read),
            cache_creation_tokens: Some(write),
            output_tokens: Some(output),
            ..Usage::default()
        }
    }

    fn hit(key: Option<&str>, h: Harness, model: &str, u: Usage) -> UsageHit {
        UsageHit {
            key: key.map(Into::into),
            harness: h,
            model: model.into(),
            usage: u,
        }
    }

    #[test]
    fn tokens_fold_dedupes_keyed_calls_and_ranks_rows() {
        let mut t = TokenStats::default();
        t.fold(
            vec![
                hit(Some("claude:m1"), Harness::Claude, "opus", usage(1, 100, 10, 5)),
                // The same streamed response on its second content-block line, or in a resumed copy.
                hit(Some("claude:m1"), Harness::Claude, "opus", usage(1, 100, 10, 5)),
                hit(Some("claude:m2"), Harness::Claude, "opus", usage(2, 200, 0, 7)),
                // Unkeyed calls with equal numbers are distinct calls: never deduplicated.
                hit(None, Harness::Codex, "gpt", usage(3, 30, 0, 1)),
                hit(None, Harness::Codex, "gpt", usage(3, 30, 0, 1)),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(t.duplicates, 1);
        assert_eq!(t.rows.len(), 2);
        let opus = &t.rows[0];
        assert_eq!((opus.harness, opus.model.as_str(), opus.calls), ("claude", "opus", 2));
        assert_eq!(
            (opus.input, opus.cache_read, opus.cache_write, opus.output),
            (3, 300, 10, 12)
        );
        assert_eq!((opus.uncached(), opus.total()), (25, 325));
        let gpt = &t.rows[1];
        assert_eq!((gpt.calls, gpt.total()), (2, 68));
        let all = t.total();
        assert_eq!((all.calls, all.total(), all.uncached()), (4, 393, 33));
        assert_eq!(
            t.by_harness().iter().map(|r| r.harness).collect::<Vec<_>>(),
            ["claude", "codex"]
        );

        let v = t.to_json();
        assert_eq!(v["total"]["total"], 393);
        assert_eq!(v["by_model"][0]["model"], "opus");
        assert!(v["total"]["cost_usd"].is_null(), "no row reported a cost");
    }

    #[test]
    fn keyed_usage_keeps_first_model_and_cost_addition_order() {
        let mut t = TokenStats::default();
        let cost = |value| Usage {
            input_tokens: Some(1),
            cost_usd: Some(value),
            ..Usage::default()
        };
        t.fold(
            [
                hit(Some("claude:first"), Harness::Claude, "first-model", cost(0.1)),
                hit(Some("claude:first"), Harness::Claude, "ignored-model", cost(900.0)),
                hit(None, Harness::Claude, "first-model", cost(0.2)),
                hit(None, Harness::Claude, "first-model", cost(0.3)),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(t.duplicates, 1);
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0].model, "first-model");
        assert_eq!(t.rows[0].calls, 3);
        assert_eq!(t.rows[0].cost_usd.unwrap().to_bits(), ((0.1_f64 + 0.2) + 0.3).to_bits());
        assert_ne!(
            t.rows[0].cost_usd.unwrap().to_bits(),
            (0.1_f64 + (0.2 + 0.3)).to_bits(),
            "grouping file costs is not an exact aggregate"
        );
    }

    #[test]
    fn usage_spill_keeps_optional_fields_cost_bits_and_eof_model_order() {
        let mut spool = Ordered::default();
        let mut oracle = TokenStats::default();
        let mut expected = Vec::new();
        for i in 0..9000 {
            let model = if i % 3 == 1 { "late-model" } else { "" };
            let cost = [0.1, 0.2, 0.3][i % 3];
            let usage = Usage {
                input_tokens: Some(1),
                cache_read_tokens: Some(0),
                reasoning_tokens: Some(3),
                cost_usd: Some(cost),
                ..Usage::default()
            };
            spool
                .push(hit(Some(&format!("id-{i}")), Harness::Claude, model, usage.clone()))
                .unwrap();
            expected.push(hit(Some(&format!("id-{i}")), Harness::Claude, "late-model", usage));
        }
        assert!(spool.temporary_bytes().unwrap() > 0);
        oracle.fold(expected.into_iter()).unwrap();
        let mut fold = TokenFold::default();
        for row in spool.rows().unwrap() {
            let mut row = row.unwrap();
            assert_eq!(row.usage.cache_read_tokens, Some(0));
            assert_eq!(row.usage.output_tokens, None);
            if row.model.is_empty() {
                row.model = "late-model".into();
            }
            fold.add(row).unwrap();
        }
        let mut actual = TokenStats::default();
        fold.finish(&mut actual);
        assert_eq!(
            serde_json::to_vec(&actual.to_json()).unwrap(),
            serde_json::to_vec(&oracle.to_json()).unwrap()
        );
        assert_eq!(
            actual.rows[0].cost_usd.unwrap().to_bits(),
            oracle.rows[0].cost_usd.unwrap().to_bits()
        );
        // The staging codec preserves the value itself, including signed zero and hard rounding.
        for cost in [-0.0, f64::from_bits(0x3fb999999999999b), f64::MIN_POSITIVE] {
            let row = hit(
                None,
                Harness::Codex,
                "",
                Usage {
                    cost_usd: Some(cost),
                    ..Usage::default()
                },
            );
            let mut bytes = Vec::new();
            row.write(&mut bytes).unwrap();
            let decoded = UsageHit::read(&mut std::io::Cursor::new(bytes), &mut None).unwrap();
            assert_eq!(decoded.usage.cost_usd.unwrap().to_bits(), cost.to_bits());
        }
    }

    // Frozen pre-spill compute recipe: this oracle exercises the original parallel helpers and
    // whole-transcript vectors, rather than comparing two runs of the new streaming fold.
    fn legacy_compute(refs: &[SessionRef]) -> TokenStats {
        let mut all = refs.to_vec();
        let subs = crate::par_flat_map(refs.to_vec(), |r| {
            crate::subagent_tree_of(&r).into_iter().map(|s| s.session).collect()
        });
        let subagents = subs.len();
        all.extend(subs);
        let total = all.len();
        let parsed = crate::par_filter_map(all, |r| {
            let adapter = crate::harness::for_harness(r.harness)?;
            let mut hits = Vec::new();
            let session = adapter
                .try_stream(&r, &ParseOptions::lazy_extra(), &mut |m: Message| {
                    if let Some(usage) = m
                        .usage
                        .as_ref()
                        .filter(|u| u.total_tokens() > 0 || u.cost_usd.is_some())
                    {
                        hits.push(UsageHit {
                            key: usage_key(r.harness, &m),
                            harness: r.harness,
                            model: m.model.clone().unwrap_or_default(),
                            usage: usage.clone(),
                        });
                    }
                    Flow::Continue
                })
                .ok()?;
            for hit in &mut hits {
                if hit.model.is_empty() {
                    hit.model = session.model.clone().unwrap_or_else(|| "(unknown)".into());
                }
            }
            Some(hits)
        });
        let mut stats = TokenStats {
            sessions: refs.len(),
            subagents,
            failed: total - parsed.len(),
            ..TokenStats::default()
        };
        let mut seen = std::collections::HashSet::new();
        let mut rows = HashMap::new();
        for hit in parsed.into_iter().flatten() {
            if let Some(key) = hit.key {
                if !seen.insert(key) {
                    stats.duplicates += 1;
                    continue;
                }
            }
            let harness = hit.harness.as_str();
            rows.entry((harness, hit.model.clone()))
                .or_insert_with(|| TokenRow {
                    harness,
                    model: hit.model,
                    ..TokenRow::default()
                })
                .add(&hit.usage);
        }
        stats.rows = rows.into_values().collect();
        stats.rows.sort_by(|a, b| {
            b.total()
                .cmp(&a.total())
                .then(a.harness.cmp(b.harness))
                .then(a.model.cmp(&b.model))
        });
        stats
    }

    #[test]
    fn old_parallel_compute_matches_multiple_transcript_precedence_and_cost_bits() {
        let world = crate::test_world("token-stats-parallel-oracle");
        let mut refs = Vec::new();
        for (i, model) in ["first", "ignored", "ignored-again"].into_iter().enumerate() {
            let value = json!({"padding":"x".repeat(64 * 1024),"type":"assistant","uuid":format!("a{i}"),"message":{
                "id":"shared-response","role":"assistant","model":model,
                "content":[{"type":"text","text":"answer"}],"usage":{"input_tokens":i + 1,"output_tokens":2}}});
            let path = world.write_session(&format!("claude-{i}"), &[value]);
            refs.push(SessionRef::new(format!("claude-{i}"), Harness::Claude, path));
        }
        let storage = world.home.join(".local/share/opencode/storage");
        for (i, cost) in [0.1, 0.2, 0.3, 0.1, 0.2, 0.3].into_iter().enumerate() {
            let id = format!("oc-{i}");
            let mid = format!("msg-{i}");
            let session = storage.join("session").join(format!("{id}.json"));
            std::fs::create_dir_all(session.parent().unwrap()).unwrap();
            std::fs::write(&session, "{}").unwrap();
            let message = storage.join("message").join(&id).join("one.json");
            std::fs::create_dir_all(message.parent().unwrap()).unwrap();
            std::fs::write(
                &message,
                json!({"id":mid,"role":"assistant","modelID":"sum-model",
                "cost":cost,"time":{"created":1},"tokens":{"input":1,"output":1}})
                .to_string(),
            )
            .unwrap();
            let part = storage.join("part").join(&mid).join("one.json");
            std::fs::create_dir_all(part.parent().unwrap()).unwrap();
            std::fs::write(part, r#"{"type":"text","text":"answer"}"#).unwrap();
            refs.push(SessionRef::new(id, Harness::OpenCode, session));
        }
        refs.push(SessionRef::new(
            "missing",
            Harness::Claude,
            world.base.join("absent.jsonl"),
        ));
        let check = || {
            let old = legacy_compute(&refs);
            let new = TokenStats::compute(&refs).unwrap();
            assert_eq!(
                serde_json::to_vec(&new.to_json()).unwrap(),
                serde_json::to_vec(&old.to_json()).unwrap()
            );
            assert_eq!(new.duplicates, 2);
            assert_eq!(new.failed, 1);
            let old_cost = old
                .rows
                .iter()
                .find(|r| r.harness == "opencode")
                .unwrap()
                .cost_usd
                .unwrap();
            let new_cost = new
                .rows
                .iter()
                .find(|r| r.harness == "opencode")
                .unwrap()
                .cost_usd
                .unwrap();
            assert_eq!(new_cost.to_bits(), old_cost.to_bits());
            assert_eq!(new.rows.iter().find(|r| r.harness == "claude").unwrap().model, "first");
        };
        #[cfg(feature = "parallel")]
        for workers in [1, 2, 4] {
            rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap()
                .install(crate::env::carry(&check));
        }
        #[cfg(not(feature = "parallel"))]
        check();
        #[cfg(feature = "sqlite")]
        for r in &refs[..3] {
            assert!(usage_cache::Cache::new()
                .unwrap()
                .load(r, &crate::source::Probe::default().snapshot(r))
                .is_some());
        }
        refs.reverse();
        let reordered = TokenStats::compute(&refs).unwrap();
        assert_eq!(reordered.to_json(), legacy_compute(&refs).to_json());
        #[cfg(feature = "sqlite")]
        {
            // Damaged persistent bytes must restart the entire ordered fold, including IDs
            // already encountered in earlier files, and replace the cache from real sources.
            let r = refs
                .iter()
                .find(|r| r.harness == Harness::Claude && r.id != "missing")
                .unwrap();
            crate::catalog::open_db()
                .unwrap()
                .execute(
                    "UPDATE usage_chunks SET payload=zeroblob(65537) WHERE harness=?1 AND id=?2 AND path=?3",
                    rusqlite::params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
                )
                .unwrap();
            assert_eq!(
                TokenStats::compute(&refs).unwrap().to_json(),
                legacy_compute(&refs).to_json()
            );
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn tiny_native_sources_parse_without_publishing_usage_cache() {
        let world = crate::test_world("token-stats-tiny");
        let path = world.write_session(
            "tiny",
            &[json!({"type":"assistant","message":{
            "id":"one","role":"assistant","model":"small","content":[],
            "usage":{"input_tokens":3,"output_tokens":7}}})],
        );
        let r = SessionRef::new("tiny", Harness::Claude, path);
        let expected = legacy_compute(std::slice::from_ref(&r)).to_json();
        for _ in 0..2 {
            assert_eq!(
                TokenStats::compute(std::slice::from_ref(&r)).unwrap().to_json(),
                expected
            );
        }
        assert!(usage_cache::Cache::new()
            .unwrap()
            .load(&r, &crate::source::Probe::default().snapshot(&r))
            .is_none());
    }

    #[test]
    fn tokens_compute_reads_claude_message_ids_across_transcripts_and_subagents() {
        // Two lines of one streamed response in the parent, the same response again in a resumed
        // copy, and a sub-agent's own call: three distinct calls' worth of lines, two calls counted
        // from the catalog sessions plus one from the sub-agent forest.
        let world = crate::test_world("token-stats");
        let root = world.base.join("token-fixtures");
        let proj = root.join("-enc");
        let subs = proj.join("sess").join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        let line = |uuid: &str, id: &str, model: &str, text: &str| {
            format!(
                r#"{{"padding":"{}","type":"assistant","uuid":"{uuid}","timestamp":"2026-09-01T00:00:00Z","message":{{"id":"{id}","role":"assistant","model":"{model}","content":[{{"type":"text","text":"{text}"}}],"usage":{{"input_tokens":3,"cache_read_input_tokens":1000,"cache_creation_input_tokens":20,"output_tokens":7}}}}}}"#,
                "x".repeat(64 * 1024)
            )
        };
        let parent = proj.join("sess.jsonl");
        let resumed = proj.join("resumed.jsonl");
        let sub = subs.join("agent-a1.jsonl");
        std::fs::write(
            &parent,
            [line("u1", "msg_A", "opus", "a"), line("u2", "msg_A", "opus", "b")].join("\n"),
        )
        .unwrap();
        std::fs::write(&resumed, line("u1", "msg_A", "opus", "a")).unwrap();
        std::fs::write(&sub, line("s1", "msg_S", "haiku", "c")).unwrap();
        let sref = |id: &str, path: &std::path::Path| SessionRef {
            id: id.into(),
            harness: Harness::Claude,
            path: path.into(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        let t = TokenStats::compute(&[sref("sess", &parent), sref("resumed", &resumed)]).unwrap();
        let warm = TokenStats::compute(&[sref("sess", &parent), sref("resumed", &resumed)]).unwrap();
        assert_eq!(warm.to_json(), t.to_json());
        std::fs::remove_dir_all(&root).ok();

        assert_eq!((t.sessions, t.subagents, t.failed), (2, 1, 0));
        assert_eq!(t.duplicates, 2, "msg_A's second line and its resumed copy");
        let rows: Vec<(&str, u64, u64)> = t.rows.iter().map(|r| (r.model.as_str(), r.calls, r.total())).collect();
        assert_eq!(rows, [("haiku", 1, 1030), ("opus", 1, 1030)]);
    }

    #[test]
    fn an_empty_corpus_is_zeroes_and_nulls_not_an_error() {
        let v = CorpusStats::compute(&[]).to_json();
        assert_eq!(v["sessions"], 0);
        assert_eq!(v["messages"], 0);
        assert!(v["by_harness"].as_object().unwrap().is_empty());
        assert!(v["top_cwds"].as_array().unwrap().is_empty());
        assert!(v["earliest_created"].is_null() && v["latest_updated"].is_null());
    }
}
