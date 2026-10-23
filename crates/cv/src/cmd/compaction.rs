//! `cv show --kind compaction_boundary,compaction_summary` — every compaction boundary in a
//! session: when, why (trigger), the pre-compaction context size, and the summary that seeded the
//! next window.

use crate::util::{fmt_duration, fmt_int, short_id};
use anyhow::Result;
use cv_core::ir::{truncate, SessionRef};

/// Print boundaries for r. JSON and summaries include full summary text; ordinary terminal
/// rows demand only a bounded preview.
pub(crate) fn print_compactions(r: &SessionRef, summaries: bool, json: bool) -> Result<()> {
    let comps = if json || summaries {
        cv_core::compaction::detect(r, true)?
    } else {
        cv_core::compaction::detect_preview(r, 200)?
    };

    if json {
        // `compaction::row`: the same row `/api/session/{h}/{id}/compactions` serves.
        let rows: Vec<serde_json::Value> = (0..comps.len()).map(|i| cv_core::compaction::row(&comps, i)).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if comps.is_empty() {
        println!("{} never compacted (0 boundaries)", short_id(&r.id));
        return Ok(());
    }

    println!("# {} compacted {} time(s)\n", short_id(&r.id), comps.len());
    for (i, c) in comps.iter().enumerate() {
        let trig = c.trigger.as_deref().unwrap_or("?");
        let pre = c
            .pre_tokens
            .map(|t| format!("{} tokens", fmt_int(t)))
            .unwrap_or_else(|| "? tokens".into());
        let dur = c
            .duration_ms
            .map(|ms| format!(" · took {}", fmt_duration(ms)))
            .unwrap_or_default();
        let span = cv_core::compaction::pre_compaction_span(&comps, i)
            .map(|(s, e)| format!("  · pre-span msgs {s}-{e}"))
            .unwrap_or_default();
        println!(
            "── #{} · {} · {} (pre) · @msg {}{}{}",
            i + 1,
            trig,
            pre,
            c.boundary_msg_idx,
            dur,
            span,
        );
        match (&c.summary, summaries) {
            (Some(s), true) => println!("\n{s}\n"),
            (Some(s), false) => println!("   summary: {}\n", truncate(s, 200)),
            (None, _) => println!("   (no summary recorded)\n"),
        }
    }
    if !summaries {
        println!(
            "→ `--summaries` for full summary text · `cv show {} --pre-compaction` to read the lost span",
            short_id(&r.id)
        );
    }
    Ok(())
}
