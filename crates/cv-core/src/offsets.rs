//! Per-message byte offsets: the stamp, not a store.
//!
//! Under [`Demand::with_offsets`](crate::Demand::with_offsets) the claude and codex
//! JSONL adapters stamp each message with the byte offset of the record line that produced it
//! ([`OFFSET_KEY`]). `cv rewind` reads the stamp to map a message index to the byte where its
//! record starts, so it can cut the transcript there.
//!
//! Offsets are stamped during the current parse, not loaded from a persisted seek index.
//! An index keyed only by file identity cannot account for changes in adapter message boundaries.

/// `Message.extra` key the claude/codex adapters stamp with the message's source byte offset
/// (the start of the JSON record line that produced it) under
/// [`Demand::with_offsets`](crate::Demand::with_offsets).
pub const OFFSET_KEY: &str = "cv_byte_offset";
