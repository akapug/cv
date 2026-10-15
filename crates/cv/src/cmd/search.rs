//! `cv search` / `cv index` — full-text and semantic search.

use crate::util::{dirs_home, parse_harness, short_id};
use anyhow::{Context, Result};
use cv_core::ir::{truncate, Harness};
use cv_core::sanitize::sanitize_line;
use std::io::Write;
use std::path::PathBuf;

pub(crate) fn cmd_search(
    query: &str,
    harness: Option<String>,
    limit: usize,
    semantic: bool,
    json: bool, // emit the hits as one JSON array (session rows + score/snippet/provenance) instead of the table
) -> Result<()> {
    let want = parse_harness(&harness)?;

    // Semantic search: embed the query and rank stored vectors. Requires `cv index --semantic`.
    if semantic {
        let hits = cv_search::semantic_search_filtered(
            None,
            query,
            limit,
            cv_search::SearchFilter {
                harness: want.map(|h| h.as_str()),
                cwd: None,
            },
        )
        .context("semantic search failed (run `cv index --semantic` first?)")?;
        render_search_hits(hits, want, limit, query, "semantic", json)?;
        return Ok(());
    }

    // A retired sqlite FTS index may still be sitting on disk from older versions; tantivy is
    // canonical now, so let the user know it's safe to remove.
    if let Some(legacy) = legacy_sqlite_index_path() {
        if legacy.exists() {
            eprintln!(
                "(note: legacy sqlite index no longer used; safe to delete {})",
                legacy.display()
            );
        }
    }

    // Preferred path: the tantivy full-text index (real tokenization + BM25). Authoritative when
    // present — an empty result means "no match", not "fall back to a live scan".
    if cv_search::default_tantivy_dir().exists() {
        match cv_search::text_search_indexed_filtered(
            None,
            query,
            limit,
            cv_search::SearchFilter {
                harness: want.map(|h| h.as_str()),
                cwd: None,
            },
        ) {
            Ok(hits) => {
                render_search_hits(hits, want, limit, query, "index", json)?;
                return Ok(());
            }
            Err(e) if cv_search::is_invalid_text_query(&e) => return crate::util::usage(format!("{e:#}")),
            Err(e) => eprintln!("(tantivy index unavailable: {e:#}; scanning live)"),
        }
    } else {
        eprintln!("(no index yet — scanning live; run `cv index` for instant search)");
    }
    cmd_search_live(query, want, limit, json)
}

/// Where the retired sqlite FTS index used to live: `$CLUSTERVISION_HOME/index.sqlite` or
/// `~/.clustervision/index.sqlite`. Only used to nudge cleanup of a stale file.
fn legacy_sqlite_index_path() -> Option<PathBuf> {
    cv_core::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".clustervision")))
        .map(|d| d.join("index.sqlite"))
}

/// Render a slice of cv-search [`cv_search::Hit`]s with harness/short-id/date/title/snippet,
/// applying the `--harness` filter and `--limit`. `source` labels the empty-result hint.
/// With `json`, the SAME hits in the same order go to stdout as one JSON array instead —
/// no hint lines, an empty result is `[]` — so the output pipes cleanly.
fn render_search_hits(
    mut hits: Vec<cv_search::Hit>,
    want: Option<Harness>,
    limit: usize,
    query: &str,
    source: &str,
    json: bool,
) -> Result<()> {
    hits.retain(|h| want.is_none_or(|w| h.harness == w.as_str()));
    hits.truncate(limit);
    if source == "index" {
        cv_search::enrich_text_snippets(None, query, &mut hits)?;
    }
    // How far the index trails the corpus right now — named on both hits and misses (a hit list
    // can be missing rows, not only an empty one). Under --json it goes to stderr, keeping stdout
    // a pure array; `cv mcp` relays stderr to the caller as its own content block.
    let stale_note = (source == "index")
        .then(|| cv_search::fts::staleness_note(&cv_search::default_tantivy_dir()))
        .flatten();
    if json {
        if let Some(note) = &stale_note {
            eprintln!("(note: {note})");
        }
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let rows = std::cell::LazyCell::new(cv_core::rows::SearchRowContext::new);
        write_json_rows(&mut out, &hits, |hit| hit_json(hit, &rows))?;
        out.flush()?;
        return Ok(());
    }
    if hits.is_empty() {
        let hint = if source == "semantic" {
            "(semantic; run `cv index --semantic` to (re)build embeddings)"
        } else {
            "(index; try `cv index` to refresh)"
        };
        println!("no matches for {query:?} {hint}");
        // The likely reason a recent conversation isn't findable: the index has fallen behind.
        if source == "index" {
            if let Some(note) = &stale_note {
                println!("(note: {note})");
            }
            // The other common miss: the wanted text lives in a *sub-agent* transcript, which the
            // default index doesn't fold in. Say so — a search that came back empty for something an
            // Agent/Workflow lane discussed is exactly this case.
            if !cv_search::fts::has_subagent_docs(&cv_search::default_tantivy_dir()) {
                println!(
                    "(note: sub-agent transcripts aren't searched yet — rebuild with \
                     `cv index --subagents` to include the Agent/Workflow lanes)"
                );
            }
        }
        return Ok(());
    }
    for h in &hits {
        // Dates ride on the hit straight from the index (FTS); semantic hits carry none.
        let date = h
            .updated_at
            .or(h.created_at)
            .and_then(|t| crate::util::fmt_local_ts(t, "%Y-%m-%d"))
            .unwrap_or_else(|| "----------".into());
        // A folded-in sub-agent hit (`cv index --subagents`): its own id is `agent-<hex>`, so a
        // bare `short_id` would render the shared `agent-` prefix, useless for drill-in. Show the
        // bare agent id (what `cv show <id>` resolves) and tag the row with its parent + workflow so
        // the reader sees it's a lane, not a top-level session.
        let (id_disp, provenance) = match h.agent_id.as_deref() {
            Some(aid) => {
                let parent = h.parent_id.as_deref().map(short_id).unwrap_or_default();
                let wf = h.workflow.as_deref().map(|w| format!(" · ⟐{w}")).unwrap_or_default();
                (short_id(aid), format!("   ⤷ sub-agent of {parent}{wf}"))
            }
            None => (short_id(&h.id), String::new()),
        };
        // Titles and snippets are transcript-derived (untrusted) — sanitize at the terminal
        // seam (G5); JSON surfaces stay raw.
        println!(
            "{:8}  {:8}  {:10}  {}{}",
            h.harness,
            id_disp,
            date,
            sanitize_line(h.title.as_deref().unwrap_or_default()),
            provenance,
        );
        if !h.snippet.trim().is_empty() {
            println!("          … {}", truncate(&sanitize_line(&h.snippet), 120));
        }
    }
    // The disclosure on the hit path: a stale index may be missing the rows the query would have
    // caught, and a hit list that doesn't say so reads as complete.
    if let Some(note) = &stale_note {
        println!("(note: {note})");
    }
    Ok(())
}

// Preserve the shared row contract and exact pretty-array formatting, retaining only one
// projected row at a time rather than another complete Value array and serialized String.
struct JsonRows<'a, T, F> {
    rows: &'a [T],
    row: F,
}
impl<T, F: Fn(&T) -> serde_json::Value> serde::Serialize for JsonRows<'_, T, F> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.rows.len()))?;
        for row in self.rows {
            sequence.serialize_element(&(self.row)(row))?;
        }
        sequence.end()
    }
}
fn write_json_rows<T>(out: &mut impl Write, rows: &[T], row: impl Fn(&T) -> serde_json::Value) -> Result<()> {
    serde_json::to_writer_pretty(&mut *out, &JsonRows { rows, row })?;
    writeln!(out)?;
    Ok(())
}

/// One `--json` object for an index/semantic hit — [`cv_core::rows::SearchRow`] does the shaping,
/// so `cv search --json` and `cv serve`'s `/api/search` emit the identical row for the identical hit.
fn hit_json(h: &cv_search::Hit, rows: &cv_core::rows::SearchRowContext) -> serde_json::Value {
    cv_core::rows::SearchRow {
        id: &h.id,
        harness: &h.harness,
        cwd: h.cwd.as_deref(),
        title: h.title.as_deref(),
        created_at: h.created_at,
        updated_at: h.updated_at,
        score: Some(h.score),
        snippet: &h.snippet,
        agent_id: h.agent_id.as_deref(),
        parent_id: h.parent_id.as_deref(),
        workflow: h.workflow.as_deref(),
    }
    .to_json_with(rows)
}

fn cmd_search_live(query: &str, want: Option<Harness>, limit: usize, json: bool) -> Result<()> {
    // The scan itself is [`cv_core::scan::live_search`] (head-capped per session, so peak memory is
    // O(cap) not O(session)) — the same call `cv serve`'s `/api/search` degrades to when the index is
    // missing, so the no-index answer is identical through both doors.
    let found = cv_core::scan::live_search(query, want, limit)?;
    if json {
        // Pure JSON on stdout even for a miss: an empty array, no prose.
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        write_json_rows(&mut out, &found.hits, |h| {
            cv_core::rows::live_search_row(&h.session, &h.title, &h.snippet)
        })?;
        out.flush()?;
        if found.truncated {
            eprintln!("(stopped at {limit} hits; use --limit)");
        }
        return Ok(());
    }
    for h in &found.hits {
        println!(
            "{:8}  {:8}  {:10}  {}",
            h.session.harness.as_str(),
            short_id(&h.session.id),
            h.session
                .updated_at
                .map(|d| crate::util::fmt_local(d, "%Y-%m-%d"))
                .unwrap_or_else(|| "----------".into()),
            sanitize_line(&h.title),
        );
        println!("          … {}", sanitize_line(&h.snippet));
    }
    if found.truncated {
        println!("\n(stopped at {limit} hits; use --limit)");
    } else if found.hits.is_empty() {
        println!("no matches for {query:?}");
    }
    Ok(())
}

pub(crate) fn cmd_index(semantic: bool, rebuild: bool, subagents: bool) -> Result<()> {
    if semantic && subagents {
        return crate::util::usage("--semantic indexes top-level sessions; --subagents currently applies only to full-text indexing. Run `cv index --subagents` and `cv index --semantic` separately.");
    }

    eprintln!(
        "✦ {} full-text index{}…",
        if rebuild { "rebuilding" } else { "updating" },
        if subagents { " (+ sub-agent forest)" } else { "" }
    );
    let (n, refs) = cv_search::index_all(None, rebuild, subagents)?;
    println!(
        "indexed {n} top-level session(s) → {}{}",
        cv_search::default_tantivy_dir().display(),
        if subagents {
            " (sub-agent transcripts folded in)"
        } else {
            ""
        }
    );
    if semantic {
        eprintln!("✦ refreshing semantic vectors (downloads a small model on first use)…");
        let e = cv_search::embed_sessions(None, &refs)?;
        println!(
            "embedded {e} session(s) → {}",
            cv_search::default_embeddings_path().display()
        );
    }
    println!("events: extracted on the same pass → try `cv events <id>` / `cv touched <path>`");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_semantic_forest_combination_fails_before_index_work() {
        for rebuild in [false, true] {
            let error = cmd_index(true, rebuild, true).unwrap_err();
            assert!(error.is::<crate::util::UsageError>());
            assert!(error.to_string().contains("top-level"));
            assert!(error.to_string().contains("cv index --subagents"));
            assert!(error.to_string().contains("cv index --semantic"));
        }
    }

    #[test]
    fn streamed_search_json_preserves_exact_pretty_row_array() {
        for rows in [
            Vec::new(),
            vec![
                serde_json::json!({"id":"one","text":"雪\n\""}),
                serde_json::json!({"id":"two","score":null,"fields":[1,true,"é"]}),
            ],
        ] {
            let mut output = Vec::new();
            write_json_rows(&mut output, &rows, Clone::clone).unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!("{}\n", serde_json::to_string_pretty(&rows).unwrap())
            );
        }
        let rows = vec![1, 2, 3];
        let visits = std::cell::RefCell::new(Vec::new());
        let mut out = Vec::new();
        write_json_rows(&mut out, &rows, |row| {
            visits.borrow_mut().push(*row);
            serde_json::json!(row)
        })
        .unwrap();
        assert_eq!(*visits.borrow(), rows);
    }
}
