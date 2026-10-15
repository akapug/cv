//! The no-index live scan: substring search over the corpus with no search index at all.
//!
//! This is the *degraded* path — `cv search` takes it before `cv index` has ever run, and `cv serve`'s
//! `/api/search` takes it when the tantivy index is missing or unreadable, so a fresh machine and
//! a broken index both still answer a query instead of erroring. It is slower than the index by
//! orders of magnitude (it streams every session) and it only sees each session's capped head, so
//! callers say so: the CLI prints a `(no index yet — scanning live…)` note, the daemon sets
//! `X-Cv-Search-Source: live`.
//!
//! Streaming discipline: each session streams into [`CapSink`], which keeps at most
//! [`LIVE_HAY_BYTES`] of searchable text and drops every message as it passes. Peak memory per
//! session is O(cap), never O(session) — an unbounded haystack here once meant multi-GB RSS on a
//! real corpus.

use crate::ir::{truncate, Block, Harness, Message, Role, SessionRef};
use crate::lazy::{Resolver, Text};
use crate::projection::Head;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use std::path::Path;

/// Searchable-haystack cap per session for the no-index live scan.
pub const LIVE_HAY_BYTES: usize = 256 * 1024;

/// Builds a capped searchable head (`hay`, at most [`LIVE_HAY_BYTES`]) as messages stream past
/// (each dropped immediately), grabbing the first user text for the label fallback; stops the
/// parse once the cap is hit and a first-user label candidate exists. Stream it under
/// [`ParseOptions::lazy`] so large content arrives as spans resolved only up to the remaining
/// budget — peak per session is O(cap), never O(session).
///
/// The no-index live path for `cv pack`, `cv search` and `cv serve`'s `/api/search` alike.
pub struct CapSink {
    pub hay: Head,
    pub first_user: Option<String>,
    resolver: Resolver,
    error: Option<anyhow::Error>,
    failed: bool,
}

impl CapSink {
    /// A fresh sink for one session at `path` (the resolver reads lazy spans back from it).
    pub fn new(path: &Path) -> Self {
        CapSink {
            hay: Head::new(LIVE_HAY_BYTES),
            first_user: None,
            resolver: Resolver::new(Some(path.to_path_buf())),
            error: None,
            failed: false,
        }
    }

    fn push_text(&mut self, text: &Text) -> Result<()> {
        self.hay.try_push_text(text, &self.resolver)?;
        self.hay.push("\n");
        Ok(())
    }
}

impl MessageSink for CapSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("live search source read failed")))
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        if self.first_user.is_none() && m.role == Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    // Inline text keeps its full non-whitespace test. Lazy labels use the same
                    // checked 512-byte head as indexed/semantic search: raw bytes for spans,
                    // decoded UTF8 bytes for retained source strings.
                    let t = match text.try_resolve_prefix(&self.resolver, 512) {
                        Ok(text) => text,
                        Err(error) => {
                            self.error = Some(error);
                            self.failed = true;
                            return Flow::Stop;
                        }
                    };
                    if !t.trim().is_empty() {
                        self.first_user = Some(t.chars().take(73).collect());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            let result = match b {
                Block::Text { text } | Block::Thinking { text, .. } => self.push_text(text),
                Block::ToolUse { name, input, .. } => {
                    self.hay.push(name);
                    self.hay.push(" ");
                    self.hay.push_json(input).map(|()| self.hay.push("\n"))
                }
                Block::ToolResult { content, .. } => self.push_text(content),
                Block::File { path, source, .. } => {
                    if let Some(p) = path.as_deref().or(source.as_deref()) {
                        self.hay.push(p);
                        self.hay.push("\n");
                    }
                    Ok(())
                }
                Block::Image { .. } => Ok(()),
            };
            if let Err(error) = result {
                self.error = Some(error);
                self.failed = true;
                return Flow::Stop;
            }
        }
        if self.hay.full() && self.first_user.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// One live-scan match: the discovered ref, the label to show for it, and the excerpt around the
/// first occurrence of the needle.
pub struct LiveHit {
    pub session: SessionRef,
    pub title: String,
    pub snippet: String,
}

/// The result of a live scan: the hits in discovery order, and whether the scan stopped early
/// because it filled `limit` (so a caller can say "there may be more").
pub struct LiveSearch {
    pub hits: Vec<LiveHit>,
    pub truncated: bool,
}

/// Case-insensitive substring search across every session's capped head, stopping at `limit` hits.
///
/// Trade-off, stated plainly: a match beyond a session's capped head is missed here; finding those
/// is the index's job (`cv index`).
pub fn live_search(query: &str, want: Option<Harness>, limit: usize) -> Result<LiveSearch> {
    live_search_filtered(query, want, limit, |_| true)
}

/// Live search with caller metadata selection applied before payload reads and the hit budget.
pub fn live_search_filtered(
    query: &str,
    want: Option<Harness>,
    limit: usize,
    mut select: impl FnMut(&crate::SessionRef) -> bool,
) -> Result<LiveSearch> {
    let _sources = crate::source::read_scope();
    let needle = query.to_lowercase();
    let mut out = LiveSearch {
        hits: Vec::new(),
        truncated: false,
    };
    if limit == 0 {
        return Ok(out);
    }
    for adapter in crate::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.roots().is_empty() {
            continue;
        }
        for r in adapter.discover()? {
            if !select(&r) {
                continue;
            }
            let mut sink = CapSink::new(&r.path);
            let meta = adapter
                .try_stream(&r, &ParseOptions::lazy(), &mut sink)
                .with_context(|| format!("scanning {}:{}", r.harness, r.id))?;
            // Lowercase only the bounded head (the match check), windowing the snippet from the
            // original-case haystack.
            let low = sink.hay.as_str().to_lowercase();
            let Some(pos) = low.find(&needle) else { continue };
            out.hits.push(LiveHit {
                title: crate::label_from(meta.title.as_deref(), sink.first_user.as_deref()),
                snippet: {
                    let range = lowercase_source_range(sink.hay.as_str(), pos..pos + needle.len());
                    snippet(sink.hay.as_str(), range.start, range.len())
                },
                session: r,
            });
            if out.hits.len() >= limit {
                out.truncated = true;
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// Map a byte range in `source.to_lowercase()` back to enclosing original UTF-8 boundaries.
/// Matching must still use whole-string lowercase: Greek final sigma depends on its context.
/// Per-character lowercase lengths are sufficient for mapping because sigma's contextual forms
/// have equal encoded lengths. Matches within an expansion (such as İ → i + combining dot)
/// include the complete original character.
pub fn lowercase_source_range(source: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    if source.is_ascii() {
        return range.start.min(source.len())..range.end.min(source.len());
    }
    let mut folded = 0;
    let mut start = source.len();
    let mut end = source.len();
    for (offset, ch) in source.char_indices() {
        let width: usize = ch.to_lowercase().map(char::len_utf8).sum();
        if range.start >= folded && range.start < folded + width {
            start = offset;
        } else if range.start == folded + width {
            start = offset + ch.len_utf8();
        }
        if range.end <= folded {
            end = offset;
            break;
        }
        if range.end <= folded + width {
            end = offset + ch.len_utf8();
            break;
        }
        folded += width;
    }
    start..end
}

/// A one-line excerpt of `hay` around `pos`, with ~40 bytes of lead-in and lead-out.
pub fn snippet(hay: &str, pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(40);
    let end = pos.saturating_add(len).saturating_add(40).min(hay.len());
    let s = &hay[floor_char(hay, start)..ceil_char(hay, end)];
    truncate(&s.replace('\n', " "), 120)
}

/// Round `i` down/up to the nearest char boundary of `s` (both clamp to `s.len()` first).
pub fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// See [`floor_char`].
pub fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_live_window_skips_sources_and_snippets_saturate_extreme_positions() {
        let world = crate::test_world("live-empty-window");
        world.write_session(
            "match",
            &[serde_json::json!({"type":"user", "message":{"role":"user", "content":"match"}})],
        );
        let results = live_search("match", Some(Harness::Claude), 0).unwrap();
        assert!(results.hits.is_empty());
        assert!(!results.truncated);
        assert!(
            !world.cv_home.join("catalog.db").exists(),
            "zero window performed discovery/count work"
        );
        assert_eq!(snippet("short", usize::MAX, usize::MAX), "");
        assert_eq!(snippet("short", 0, usize::MAX), "short");
    }

    #[test]
    fn unicode_lowercase_ranges_preserve_source_matches_and_context() {
        for (source, needle, expected) in [
            ("İİ target tail", "target", "target"),
            ("ȺȺ target tail", "target", "target"),
            ("İstanbul", "i", "İ"),
            ("İstanbul", "\u{307}", "İ"),
            ("ΟΣ next", "ος", "ΟΣ"),
            ("ΟΣΑ next", "οσα", "ΟΣΑ"),
            ("ΟΣ next", "ς", "Σ"),
        ] {
            let lowered = source.to_lowercase();
            let pos = lowered.find(needle).unwrap();
            let range = lowercase_source_range(source, pos..pos + needle.len());
            assert_eq!(&source[range], expected, "{source:?} / {needle:?}");
        }
        // Old lowercased offsets omit the actual match after enough expanding characters.
        let source = format!("{}target {}", "İ".repeat(200), "end ".repeat(100));
        let lowered = source.to_lowercase();
        let pos = lowered.find("target").unwrap();
        let range = lowercase_source_range(&source, pos..pos + 6);
        let original = source.find("target").unwrap();
        assert_eq!(range, original..original + 6);
        assert_eq!(
            snippet(&source, range.start, range.len()),
            truncate(&source[original - 40..original + 6 + 40], 120)
        );
        assert!(!snippet(&source, pos, 6).contains("target"));
    }

    #[test]
    fn live_head_caps_arguments_paths_and_labels_together() {
        let mut sink = CapSink::new(Path::new("unused"));
        let input = serde_json::json!({"content": "🌙\n".repeat(LIVE_HAY_BYTES)});
        let mut message = Message::new(Role::User);
        message.content = vec![
            Block::Text { text: "prompt".into() },
            Block::ToolUse {
                id: "t".into(),
                name: "Edit".into(),
                input: input.clone().into(),
                namespace: None,
            },
            Block::File {
                mime: None,
                path: Some("later".repeat(LIVE_HAY_BYTES)),
                source: None,
            },
        ];
        assert_eq!(sink.message(message), Flow::Stop);
        let complete = format!("prompt\nEdit {input}\nlater");
        assert_eq!(sink.hay.as_str(), &complete[..floor_char(&complete, LIVE_HAY_BYTES)]);
        assert_eq!(sink.first_user.as_deref(), Some("prompt"));
        assert!(sink.hay.capacity() <= LIVE_HAY_BYTES);
    }

    #[test]
    fn retained_source_text_keeps_bounded_body_label_policy_and_guard_errors() {
        let dir = cv_testkit::temp_dir("live-head-retained-source");
        let path = dir.join("source.json");
        let conditional = dir.join("sidecar");
        for late_whitespace in [false, true] {
            let decoded = format!(
                "{}{}",
                if late_whitespace {
                    " ".repeat(1024)
                } else {
                    "source label\n".into()
                },
                "é🙂".repeat(LIVE_HAY_BYTES)
            );
            std::fs::write(&path, serde_json::to_vec(&decoded).unwrap()).unwrap();
            let mut reads = crate::source::ReadSet::default();
            reads.record(&conditional);
            let value = crate::json::Json::source_with_reads(&path, 0, std::fs::metadata(&path).unwrap().len(), &reads)
                .unwrap();
            let text = value.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
            assert!(matches!(text, Text::Source(_)));
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.clone() });
            let mut sink = CapSink::new(&path);
            assert_eq!(
                sink.message(message),
                if late_whitespace { Flow::Continue } else { Flow::Stop }
            );
            assert!(sink.take_error().is_none());
            assert_eq!(sink.hay.as_str().len(), floor_char(&decoded, LIVE_HAY_BYTES));
            assert!(sink.hay.as_str() == &decoded[..floor_char(&decoded, LIVE_HAY_BYTES)]);
            assert!(sink.hay.capacity() <= LIVE_HAY_BYTES);
            assert_eq!(sink.first_user.is_none(), late_whitespace);
            std::fs::write(&conditional, format!("dependency {late_whitespace}")).unwrap();
            let mut stale = CapSink::new(&path);
            stale.hay = Head::new(3);
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::Text { text });
            assert!(stale.try_message(message).is_err());
            assert!(stale.take_error().is_some());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_text_demand_stays_latched_for_body_and_label() {
        let dir = cv_testkit::temp_dir("live-head-text-error");
        let path = dir.join("absent");
        for role in [Role::User, Role::Assistant] {
            let mut sink = CapSink::new(&path);
            let mut message = Message::new(role);
            message.content.push(Block::Text {
                text: Text::Span(crate::Span {
                    source: Some(path.clone()),
                    offset: 0,
                    len: 8192,
                    escaped: false,
                }),
            });
            assert!(sink.try_message(message).is_err());
            assert_eq!(sink.message(Message::new(Role::User)), Flow::Stop);
            assert!(sink.take_error().is_some());
            assert!(sink.hay.is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lazy_label_uses_prefix_policy_and_inline_checks_full_whitespace() {
        let dir = cv_testkit::temp_dir("live-head-lazy-label");
        let path = dir.join("text");
        let content = format!("{}late meaningful text", " ".repeat(1024));
        std::fs::write(&path, &content).unwrap();
        let mut lazy = CapSink::new(&path);
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text {
            text: Text::Span(crate::Span {
                source: Some(path.clone()),
                offset: 0,
                len: content.len() as u64,
                escaped: false,
            }),
        });
        assert_eq!(lazy.message(message), Flow::Continue);
        assert!(lazy.first_user.is_none());
        assert!(lazy.hay.as_str().contains("late meaningful text"));
        let mut inline = CapSink::new(&path);
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text { text: content.into() });
        inline.message(message);
        assert_eq!(inline.first_user.as_deref(), Some(" ".repeat(73).as_str()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_source_is_not_a_successful_short_head() {
        let dir = cv_testkit::temp_dir("live-head-source-error");
        let path = dir.join("input.json");
        std::fs::write(&path, "{}").unwrap();
        let input = crate::json::Json::source(&path, 0, 2).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut sink = CapSink::new(&path);
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "t".into(),
            name: "Edit".into(),
            input,
            namespace: None,
        });
        assert!(sink.try_message(message).is_err());
        assert_eq!(sink.message(Message::new(Role::User)), Flow::Stop);
        assert!(sink.take_error().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snippets_window_the_match_and_stay_on_char_boundaries() {
        let hay = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
        let s = snippet(&hay, 100, 6);
        assert!(s.contains("NEEDLE"), "{s}");
        assert!(s.starts_with(&"a".repeat(40)) && s.len() <= 120, "{s}");
        // Multi-byte text must not be sliced mid-codepoint.
        let uni = "日本語テキスト NEEDLE 日本語テキスト";
        let pos = uni.find("NEEDLE").unwrap();
        assert!(snippet(uni, pos, 6).contains("NEEDLE"));
        // An out-of-range position clamps to an empty window instead of panicking.
        assert_eq!(snippet("short", 900, 6), "");
    }
}
