//! Per-message byte offsets: the stamp, not a store.
//!
//! Under [`ParseOptions::lazy_offsets`](crate::ParseOptions::lazy_offsets) the claude and codex
//! JSONL adapters stamp each message with the byte offset of the record line that produced it
//! ([`OFFSET_KEY`]). `cv rewind` reads the stamp to map a message index to the byte where its
//! record starts, so it can cut the transcript there.
//!
//! There is deliberately no persisted seek index. One existed (a `msg_offsets` table filled by
//! `cv index`, read by `cv show --range/--last` to seek straight to message A). Measured on
//! 2026-10-11 against this machine's corpus, `cv show <id> --last 20`, three runs each, page cache
//! warm: the largest Claude session (21,264 messages, 139 MB) took 0.37 s with the store and 0.71 s
//! without; the largest codex session by bytes (708 MB) took 1.87 s and 3.73 s. `--last` already
//! streams the whole file once to count messages, so the store saved at most one of two passes,
//! under 2 s everywhere. And it was wrong: keyed only by file signature, it survived adapter
//! changes, and on the largest codex session by messages (70,071 recorded, 92,062 parsed today)
//! `--last 20` printed an empty window. The table is dropped on catalog open.

use std::path::Path;

/// `Message.extra` key the claude/codex adapters stamp with the message's source byte offset
/// (the start of the JSON record line that produced it) under
/// [`ParseOptions::lazy_offsets`](crate::ParseOptions::lazy_offsets).
pub const OFFSET_KEY: &str = "cv_byte_offset";

/// File `(mtime_ns, size)`: the staleness signature catalog rows are keyed by.
/// `(0, 0)` when unreadable, which is never treated as fresh.
pub fn file_sig(path: &Path) -> (i64, i64) {
    let Ok(md) = std::fs::metadata(path) else { return (0, 0) };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    (mtime, md.len() as i64)
}

/// A matching tuple, including a real empty file, may be reused only with fresh captured
/// source authority. The source guard must establish primary identity and dependencies;
/// a legacy/missing stamp or unknown nullable size cannot supply that authority.
pub fn sig_is_fresh_with_source(stored: (i64, i64), current: (i64, i64), source_fresh: impl FnOnce() -> bool) -> bool {
    current.0 != 0 && current.1 >= 0 && stored == current && source_fresh()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_empty_requires_known_tuple_and_source_authority() {
        assert!(sig_is_fresh_with_source((111, 0), (111, 0), || true));
        for (stored, current) in [
            ((111, 0), (111, 0)),
            ((0, 0), (0, 0)),
            ((111, 0), (222, 0)),
            ((111, 0), (111, 1)),
        ] {
            assert!(!sig_is_fresh_with_source(stored, current, || false));
        }
        assert!(!sig_is_fresh_with_source((0, 0), (0, 0), || true));
        assert!(!sig_is_fresh_with_source((111, -1), (111, -1), || true));
        assert!(!sig_is_fresh_with_source((111, 0), (222, 0), || panic!(
            "mismatching tuple must not probe source"
        )));
    }
}
