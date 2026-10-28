//! The current context window of a session, tracked as its messages stream past: the anchors so
//! far, the token fill from the transcript's own usage records, and the compactions the harness
//! did itself. Fed one message at a time (a full read, or a follower's appended messages), it
//! holds O(anchors) and never the transcript.

use super::anchor::{call_marker, segments, Anchor};
use super::Cut;
use crate::ir::{Block, Message, MessageKind};
use serde_json::Value;

/// An anchor and the message it came from.
#[derive(Debug, Clone)]
pub struct Placed {
    pub anchor: Anchor,
    pub index: usize,
}

/// One compaction the harness did, as its transcript recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    pub index: usize,
    pub trigger: Option<String>,
    pub pre_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    /// The harness served it from its own precomputed summary (Claude Code's
    /// `compactMetadata.precomputed`).
    pub precomputed: bool,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A session's current window.
#[derive(Debug, Clone, Default)]
pub struct Window {
    /// Messages seen in the session (the next message's index).
    pub messages: usize,
    /// Compactions seen; the window began after the last one.
    pub boundaries: Vec<Boundary>,
    /// Index of the window's first message.
    pub start: usize,
    pub anchors: Vec<Placed>,
    /// Newest usage reading: `(tokens in the window, message index)`.
    reading: Option<(u64, usize)>,
    /// The newest message's id, index and time.
    newest: Option<(Option<String>, usize)>,
    pub newest_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The model of the newest reply.
    pub model: Option<String>,
    /// Whether anything other than the last anchor's message came after the last anchor.
    after_last_anchor: bool,
    /// The anchor count before the prompts no reply has answered yet (queued prompts are one
    /// turn).
    open_prompt: Option<usize>,
}

fn bag_value<'a>(m: &'a Message, key: &str) -> Option<&'a Value> {
    m.extra
        .values()
        .filter_map(Value::as_object)
        .find_map(|bag| bag.get(key))
}

impl Window {
    /// Take the next message of the session. `resolver` resolves text left in the source.
    ///
    /// A turn whose prompts drew an error before any reply may leave the conversation: Claude Code
    /// parents the next turn above a prompt that hit a usage limit, and the same scheduled prompt
    /// can come back later, where its anchor would match out of order. So an error drops the
    /// anchors of the prompts no reply has answered yet. When the error was a retry that then
    /// succeeded, that only costs those anchors: dropping an anchor can never make a request
    /// miss, it only leaves the fingerprint shorter.
    pub fn observe(&mut self, m: &Message, resolver: &crate::lazy::Resolver) -> anyhow::Result<()> {
        let index = self.messages;
        self.messages += 1;
        if m.timestamp.is_some() {
            self.newest_at = m.timestamp;
        }
        match m.kind {
            MessageKind::CompactionBoundary => {
                let meta = bag_value(m, "compactMetadata").or_else(|| bag_value(m, "compact_metadata"));
                let field = |k: &str| meta.and_then(|v| v.get(k));
                self.boundaries.push(Boundary {
                    index,
                    trigger: field("trigger").and_then(Value::as_str).map(str::to_owned),
                    pre_tokens: field("preTokens").and_then(Value::as_u64),
                    duration_ms: field("durationMs").and_then(Value::as_u64),
                    precomputed: field("precomputed").and_then(Value::as_bool).unwrap_or(false),
                    at: m.timestamp,
                });
                self.start = index + 1;
                self.anchors.clear();
                self.reading = None;
                self.newest = None;
                self.after_last_anchor = false;
                self.open_prompt = None;
                return Ok(());
            }
            MessageKind::Prompt => {
                self.open_prompt.get_or_insert(self.anchors.len());
            }
            MessageKind::Reply => self.open_prompt = None,
            MessageKind::CompactionSummary => {}
            MessageKind::Error => {
                if let Some(before) = self.open_prompt.take() {
                    self.anchors.truncate(before);
                }
                self.newest = Some((m.id.clone(), index));
                self.after_last_anchor = true;
                return Ok(());
            }
            _ => {
                self.newest = Some((m.id.clone(), index));
                self.after_last_anchor = true;
                return Ok(());
            }
        }
        if m.kind == MessageKind::Reply {
            if m.model.is_some() {
                self.model = m.model.clone();
            }
            if let Some(u) = &m.usage {
                let tokens = u.input_tokens.unwrap_or(0)
                    + u.cache_read_tokens.unwrap_or(0)
                    + u.cache_creation_tokens.unwrap_or(0)
                    + u.output_tokens.unwrap_or(0);
                if tokens > 0 {
                    self.reading = Some((tokens, index));
                }
            }
        }
        let mut anchored = false;
        let mut other = false;
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    let mut s = String::new();
                    text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                        s.push_str(part);
                        Ok(())
                    })?;
                    for segment in segments(&s) {
                        self.anchors.push(Placed {
                            anchor: Anchor::of(&segment),
                            index,
                        });
                        anchored = true;
                        other = false;
                    }
                }
                Block::Thinking { .. } => {}
                Block::ToolUse { id, .. } => match call_marker(id) {
                    Some(marker) => {
                        self.anchors.push(Placed {
                            anchor: Anchor::of(&marker),
                            index,
                        });
                        anchored = true;
                        other = false;
                    }
                    None => other = true,
                },
                _ => other = true,
            }
        }
        if anchored {
            self.after_last_anchor = other;
        } else {
            self.after_last_anchor = true;
        }
        self.newest = Some((m.id.clone(), index));
        Ok(())
    }

    /// The window's token fill: the newest usage reading, never an estimate.
    pub fn fill(&self) -> Option<u64> {
        self.reading.map(|(t, _)| t)
    }

    pub fn newest_index(&self) -> Option<usize> {
        self.newest.as_ref().map(|(_, i)| *i)
    }

    /// The harness precomputes this session's compactions itself: its newest compaction was
    /// served from a precomputed summary. cv then stays idle for the session.
    pub fn native_precompute(&self) -> bool {
        self.boundaries.last().is_some_and(|b| b.precomputed)
    }

    /// The token count where this session's harness auto-compacted, from its own records: the
    /// lowest `preTokens` of its automatic compactions.
    pub fn compact_point(&self) -> Option<u64> {
        self.boundaries
            .iter()
            .filter(|b| b.trigger.as_deref() == Some("auto"))
            .filter_map(|b| b.pre_tokens)
            .min()
    }

    /// The cut at the newest message, without its anchors (the first [`Window::anchors`]`.len()`
    /// anchors are the cut's).
    pub fn cut(&self) -> Option<Cut> {
        let (id, index) = self.newest.clone()?;
        Some(Cut {
            index,
            id,
            tokens: self.fill(),
            clean: !self.anchors.is_empty() && !self.after_last_anchor,
        })
    }

    /// The cut at the newest message and the anchors up to it.
    pub fn cut_now(&self) -> Option<(Cut, Vec<Anchor>)> {
        Some((self.cut()?, self.anchors.iter().map(|p| p.anchor.clone()).collect()))
    }
}
