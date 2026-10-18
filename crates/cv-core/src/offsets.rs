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

/// `Message.extra` key the claude/codex adapters stamp with the message's source byte offset
/// (the start of the JSON record line that produced it) under
/// [`ParseOptions::lazy_offsets`](crate::ParseOptions::lazy_offsets).
pub const OFFSET_KEY: &str = "cv_byte_offset";
