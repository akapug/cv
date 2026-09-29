//! Corpus-wide statistics over a set of discovered sessions — what `cv stats` prints and what
//! `cvd`'s `/api/stats` serves.
//!
//! The numbers come from the catalog row alone (harness, message count, cwd, dates), so a whole
//! fleet costs one catalog read and no transcript parsing. Both doors call [`CorpusStats::compute`]
//! and [`CorpusStats::to_json`] so the dashboard's Stats view and `cv stats --json` cannot drift —
//! the dashboard used to accumulate its own totals from whichever sessions the user had clicked,
//! and said so in the UI.

use crate::ir::{Harness, Message, SessionRef, Usage};
use crate::stream::{Flow, ParseOptions};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

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
    /// Subset of `output`.
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
fn usage_key(h: Harness, m: &Message) -> Option<String> {
    match h {
        Harness::Claude => m
            .harness_extra(Harness::Claude)
            .and_then(|b| b.get("message.id"))
            .and_then(Value::as_str)
            .map(|id| format!("claude:{id}")),
        _ => None,
    }
}

fn usage_hits(r: &SessionRef) -> Option<Vec<UsageHit>> {
    let adapter = crate::harness::for_harness(r.harness)?;
    let mut hits = Vec::new();
    // `lazy_extra`: content stays unread on disk, but the bag carrying Claude's `message.id` is
    // materialized.
    let session = adapter
        .stream(r, &ParseOptions::lazy_extra(), &mut |m: Message| {
            if let Some(u) = m
                .usage
                .as_ref()
                .filter(|u| u.total_tokens() > 0 || u.cost_usd.is_some())
            {
                hits.push(UsageHit {
                    key: usage_key(r.harness, &m),
                    harness: r.harness,
                    model: m.model.clone().unwrap_or_default(),
                    usage: u.clone(),
                });
            }
            Flow::Continue
        })
        .ok()?;
    for h in &mut hits {
        if h.model.is_empty() {
            h.model = session.model.clone().unwrap_or_else(|| "(unknown)".into());
        }
    }
    Some(hits)
}

impl TokenStats {
    pub fn compute(refs: &[SessionRef]) -> TokenStats {
        let mut stats = TokenStats {
            sessions: refs.len(),
            ..TokenStats::default()
        };
        let mut all: Vec<SessionRef> = refs.to_vec();
        let subs: Vec<SessionRef> = crate::par_flat_map(refs.to_vec(), |r| {
            crate::subagent_tree_of(&r).into_iter().map(|s| s.session).collect()
        });
        stats.subagents = subs.len();
        all.extend(subs);
        let total = all.len();
        let parsed: Vec<Vec<UsageHit>> = crate::par_filter_map(all, |r| usage_hits(&r));
        stats.failed = total - parsed.len();
        stats.fold(parsed.into_iter().flatten());
        stats
    }

    /// Sum `hits` into [`rows`](TokenStats::rows), dropping every keyed hit after the first.
    fn fold(&mut self, hits: impl Iterator<Item = UsageHit>) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut rows: HashMap<(&'static str, String), TokenRow> = HashMap::new();
        for hit in hits {
            if let Some(k) = hit.key {
                if !seen.insert(k) {
                    self.duplicates += 1;
                    continue;
                }
            }
            let h = hit.harness.as_str();
            rows.entry((h, hit.model.clone()))
                .or_insert_with(|| TokenRow {
                    harness: h,
                    model: hit.model,
                    ..TokenRow::default()
                })
                .add(&hit.usage);
        }
        self.rows = rows.into_values().collect();
        self.rows.sort_by(|a, b| {
            b.total()
                .cmp(&a.total())
                .then(a.harness.cmp(b.harness))
                .then(a.model.cmp(&b.model))
        });
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

/// `~/dev/x` for a path under `$HOME`, the full path otherwise. `$HOME` (not `dirs::home_dir`) so
/// a test or a daemon running under a redirected home agrees with the CLI.
fn home_rel(p: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

/// The keys `cv stats --json` / `GET /api/stats` carry, in order.
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
        );
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
    fn tokens_compute_reads_claude_message_ids_across_transcripts_and_subagents() {
        // Two lines of one streamed response in the parent, the same response again in a resumed
        // copy, and a sub-agent's own call: three distinct calls' worth of lines, two calls counted
        // from the catalog sessions plus one from the sub-agent forest.
        let root = std::env::temp_dir().join(format!("cv-tokstats-{}", uuid::Uuid::new_v4()));
        let proj = root.join("-enc");
        let subs = proj.join("sess").join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        let line = |uuid: &str, id: &str, model: &str, text: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"{uuid}","timestamp":"2026-09-01T00:00:00Z","message":{{"id":"{id}","role":"assistant","model":"{model}","content":[{{"type":"text","text":"{text}"}}],"usage":{{"input_tokens":3,"cache_read_input_tokens":1000,"cache_creation_input_tokens":20,"output_tokens":7}}}}}}"#
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
        let t = TokenStats::compute(&[sref("sess", &parent), sref("resumed", &resumed)]);
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
