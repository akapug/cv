//! The live scan: substring search over the corpus with no search index at all.
//!
//! Only an explicit request takes it: `cv search --scan` and `cv serve`'s `/api/search?scan=1`
//! (`cv pack` before any `cv index`). A missing or unusable index is an error naming the fix,
//! never this. It is slower than the index by orders of magnitude (it streams every session) and
//! only sees each session's capped head, so callers say so: the CLI prints a note on stderr, the
//! daemon sets `X-Cv-Search-Source: live`.
//!
//! Streaming discipline: each session streams into [`CapSink`], which keeps at most
//! [`LIVE_HAY_BYTES`] of searchable text and drops every message as it passes. Peak memory per
//! session is O(cap), never O(session) — an unbounded haystack here once meant multi-GB RSS on a
//! real corpus.

use crate::ir::{truncate, Block, Harness, Message, SessionRef};
use crate::lazy::{Resolver, Text};
use crate::projection::Head;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use std::path::Path;

/// Searchable-haystack cap per session for the no-index live scan.
pub const LIVE_HAY_BYTES: usize = 256 * 1024;

/// Builds a capped searchable head (`hay`, at most [`LIVE_HAY_BYTES`]) as messages stream past
/// (each dropped immediately), keeping the first [`Message::prompt_title`] for the label; stops the
/// parse once the cap is hit and the label is known. Stream it under
/// [`Demand::with_spans`] so large content arrives as spans resolved only up to the remaining
/// budget — peak per session is O(cap), never O(session).
///
/// The no-index live path for `cv pack`, `cv search` and `cv serve`'s `/api/search` alike.
pub struct CapSink {
    pub hay: Head,
    pub first_prompt: Option<String>,
    resolver: Resolver,
    error: Option<anyhow::Error>,
    failed: bool,
}

impl CapSink {
    /// A fresh sink for one session at `path` (the resolver reads lazy spans back from it).
    pub fn new(path: &Path) -> Self {
        CapSink {
            hay: Head::new(LIVE_HAY_BYTES),
            first_prompt: None,
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
        if self.first_prompt.is_none() {
            match m.prompt_title(&self.resolver) {
                Ok(title) => self.first_prompt = title,
                Err(error) => {
                    self.error = Some(error);
                    self.failed = true;
                    return Flow::Stop;
                }
            }
        }
        // Only the exchange is matched, as the index matches it: injected instruction files and
        // system prompts open nearly every session's head and would answer every query.
        if !m.kind.is_exchange() {
            return Flow::Continue;
        }
        for b in &m.content {
            let result = match b {
                Block::Text { text } | Block::Thinking { text, .. } => self.push_text(text),
                Block::ToolUse { name, input, .. } => {
                    self.hay.push(name);
                    self.hay.push(" ");
                    self.hay.push_json(input).map(|()| {
                        self.hay.push("\n");
                    })
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
        if self.hay.full() && self.first_prompt.is_some() {
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

/// The result of a live scan: the hits in discovery order, whether the scan stopped early
/// because it filled `limit` (so a caller can say "there may be more"), and the sources and
/// sessions it could not read (so a caller can say what it did not search).
pub struct LiveSearch {
    pub hits: Vec<LiveHit>,
    pub truncated: bool,
    pub unreadable: Vec<crate::Skipped>,
}

/// Case-insensitive substring search across every session's capped head, stopping at `limit` hits.
///
/// Trade-off, stated plainly: a match beyond a session's capped head is missed here; finding those
/// is the index's job (`cv index`).
///
/// Caller metadata selection is applied before payload reads and the hit budget.
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
        unreadable: Vec::new(),
    };
    if limit == 0 {
        return Ok(out);
    }
    for adapter in crate::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.roots().is_empty() {
            continue;
        }
        let found = adapter.discover()?;
        out.unreadable.extend(found.skipped);
        for r in found.refs {
            if !select(&r) {
                continue;
            }
            // One session that cannot be read is skipped and named, never the end of the scan; one
            // that vanished since discovery is simply gone.
            let mut sink = CapSink::new(&r.path);
            let meta = match adapter
                .try_stream(&r, &Demand::LEAN.with_spans(), &mut sink)
                .with_context(|| format!("scanning {}:{}", r.harness, r.id))
            {
                Ok(meta) => meta,
                Err(error) => {
                    out.unreadable.extend(crate::Skipped::of(&r.path, &error));
                    continue;
                }
            };
            // Lowercase only the bounded head (the match check), windowing the snippet from the
            // original-case haystack.
            let low = sink.hay.as_str().to_lowercase();
            let Some(pos) = low.find(&needle) else { continue };
            out.hits.push(LiveHit {
                title: crate::label_from(meta.title.as_deref(), sink.first_prompt.as_deref()),
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
    use crate::ir::Role;

    /// A session the scan cannot read is skipped and named; the rest are still searched.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_session_is_named_not_the_end_of_the_scan() {
        use std::os::unix::fs::PermissionsExt;
        let world = crate::test_world("live-unreadable");
        let line = |text: &str| serde_json::json!({"type":"user", "message":{"role":"user", "content":text}});
        let locked = world.write_session("locked", &[line("match locked")]);
        world.write_session("open", &[line("match open")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let results = live_search_filtered("match", Some(Harness::Claude), 10, |_| true);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        let results = results.unwrap();
        assert_eq!(
            results.hits.iter().map(|h| h.session.id.as_str()).collect::<Vec<_>>(),
            ["open"]
        );
        assert_eq!(
            results.unreadable.iter().map(|s| &s.path).collect::<Vec<_>>(),
            [&locked]
        );
    }

    #[test]
    fn empty_live_window_skips_sources_and_snippets_saturate_extreme_positions() {
        let world = crate::test_world("live-empty-window");
        world.write_session(
            "match",
            &[serde_json::json!({"type":"user", "message":{"role":"user", "content":"match"}})],
        );
        let results = live_search_filtered("match", Some(Harness::Claude), 0, |_| true).unwrap();
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
        assert_eq!(sink.first_prompt.as_deref(), Some("prompt"));
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
                    " ".repeat(crate::ir::TITLE_HEAD as usize)
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
            assert_eq!(sink.first_prompt.is_none(), late_whitespace);
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
