//! Live session following — the engine behind `cv scry`, the `cvd` daemon, and MCP `await_omen`.
//!
//! It polls discovery on an interval, diffs each session's message tail against what it saw last,
//! and emits [`SessionEvent`]s for new sessions and newly-appended messages. Polling (rather than
//! inotify) keeps it uniform across all harness layouts (single file, dir-of-files, sqlite).

use crate::harness;
use crate::ir::{Harness, Message, SessionRef};
use std::collections::HashMap;
use std::time::Duration;

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A session we'd never seen before appeared.
    New,
    /// An existing session gained messages.
    Updated,
}

/// A live event about a session.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub kind: EventKind,
    pub reference: SessionRef,
    /// For `New`: the whole conversation. For `Updated`: only the newly-appended messages.
    pub new_messages: Vec<Message>,
}

/// Narrow what to follow.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub harness: Option<Harness>,
    pub cwd_contains: Option<String>,
}

impl Filter {
    /// Whether a session passes this filter (harness + cwd-substring). Public so non-`Watcher`
    /// consumers (e.g. the MCP `observe_stream` tool) can reuse the exact same filtering semantics
    /// rather than re-deriving them and drifting.
    pub fn matches(&self, r: &SessionRef) -> bool {
        if let Some(h) = self.harness {
            if r.harness != h {
                return false;
            }
        }
        if let Some(needle) = &self.cwd_contains {
            let hit = r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(needle))
                .unwrap_or(false);
            if !hit {
                return false;
            }
        }
        true
    }
}

/// Where a follower stands in one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// This many parsed IR messages have already been seen.
    Messages(usize),
    /// Not parsed yet: the transcript was this many bytes long when the follower started. An
    /// append-only JSONL transcript's first `n` bytes are exactly what a parse at that moment would
    /// have read, so [`resolve`] recovers the same message count later — when, and only if, the
    /// session changes.
    Bytes(u64),
}

/// Transcripts that only ever grow by appending lines, so a byte length pins a past state: Claude
/// and plain Codex `.jsonl` (not `.jsonl.zst`, whose byte prefix is not a transcript prefix).
fn append_only(r: &SessionRef) -> bool {
    matches!(r.harness, Harness::Claude | Harness::Codex) && r.path.extension().is_some_and(|e| e == "jsonl")
}

/// Record where `r` stands now, as cheaply as possible: one `stat` for an append-only transcript,
/// a streamed count otherwise. `None` when the session can't be read.
///
/// Following "from now on" used to parse every matching session up front just to learn its
/// message count — tens of seconds and ~1 GB transient on a large corpus, almost all of it for
/// sessions that never change while followed. A byte mark defers that parse to [`resolve`].
pub fn baseline(r: &SessionRef) -> Option<Mark> {
    if append_only(r) {
        if let Ok(md) = std::fs::metadata(&r.path) {
            return Some(Mark::Bytes(md.len()));
        }
    }
    count(r).map(Mark::Messages)
}

/// The message count a full parse of `r` returns, without holding the parsed session.
///
/// It is the parse's own count by construction: the session is streamed through the adapter and
/// nothing is kept. A count taken any other way (a row count, a hand-written survival query) is a
/// second definition of "which records become messages" that has to agree with the adapter's
/// mapping forever, and a mark that disagrees with the parse makes a follower skip or repeat.
fn count(r: &SessionRef) -> Option<usize> {
    tail(r, usize::MAX, 0).map(|(total, _)| total)
}

/// What a full parse of `r` returns past the first `skip` messages: `(total, messages[skip..])`,
/// keeping at most `keep` of them. Every adapter's `parse` is its `stream` into a collecting sink,
/// so streaming and keeping only the tail gives the same messages while holding O(tail) instead of
/// the whole transcript — what a follower needs, since it only reports what was appended. Codex is
/// the exception: its `parse` is a separate whole-text path, so it is parsed and sliced as before.
pub fn tail(r: &SessionRef, skip: usize, keep: usize) -> Option<(usize, Vec<Message>)> {
    let adapter = harness::for_harness(r.harness)?;
    if r.harness == Harness::Codex {
        let messages = adapter.parse(r).ok()?.messages;
        let total = messages.len();
        return Some((total, messages.into_iter().skip(skip).take(keep).collect()));
    }
    let mut total = 0usize;
    let mut kept = Vec::new();
    let mut sink = |m: Message| {
        if total >= skip && kept.len() < keep {
            kept.push(m);
        }
        total += 1;
        crate::stream::Flow::Continue
    };
    let session = adapter
        .stream(r, &crate::stream::ParseOptions::full(), &mut sink)
        .ok()?;
    // As `stream::collect` does: own any lazy content (full-fidelity streams rarely produce any).
    let resolver = session.resolver();
    for m in &mut kept {
        m.materialize(&resolver);
    }
    Some((total, kept))
}

/// The parsed message count `mark` stands for. `None` when a byte mark's transcript can't be read.
pub fn resolve(r: &SessionRef, mark: Mark) -> Option<usize> {
    match mark {
        Mark::Messages(n) => Some(n),
        Mark::Bytes(len) => match r.harness {
            Harness::Claude => harness::claude::count_prefix(r, len).ok(),
            Harness::Codex => harness::codex::count_prefix(r, len).ok(),
            _ => None,
        },
    }
}

struct State {
    /// Cheap change signal from discovery (no full parse needed to detect change).
    trigger: (usize, Option<i64>),
    /// How far we've already reported.
    mark: Mark,
}

/// A stateful poller. Call [`poll`](Watcher::poll) repeatedly, or [`run`](Watcher::run) to loop.
pub struct Watcher {
    filter: Filter,
    seen: HashMap<String, State>,
}

impl Watcher {
    /// Create a watcher. If `emit_existing` is false, the current sessions are recorded silently so
    /// only activity *after* construction is reported (the usual `tail -f` feel).
    pub fn new(filter: Filter, emit_existing: bool) -> Self {
        let mut w = Watcher {
            filter,
            seen: HashMap::new(),
        };
        if !emit_existing {
            w.prime();
        }
        w
    }

    fn key(r: &SessionRef) -> String {
        format!("{}:{}", r.harness.as_str(), r.id)
    }

    fn trigger_of(r: &SessionRef) -> (usize, Option<i64>) {
        (r.message_count, r.updated_at.map(|t| t.timestamp_millis()))
    }

    /// Record current state without emitting anything.
    fn prime(&mut self) {
        for r in self.discover() {
            // The cheap discover `message_count` is NOT the parsed IR length for several harnesses
            // (codex/hermes/claude add reasoning/tool/system turns), so seeding from it makes the
            // first `Updated` poll re-emit messages that predate the watcher. Record the true
            // baseline instead — a byte mark where the transcript is append-only (see [`baseline`]).
            let mark = baseline(&r).unwrap_or(Mark::Messages(r.message_count));
            self.seen.insert(
                Self::key(&r),
                State {
                    trigger: Self::trigger_of(&r),
                    mark,
                },
            );
        }
    }

    fn discover(&self) -> Vec<SessionRef> {
        crate::discover_all()
            .into_iter()
            .filter(|r| self.filter.matches(r))
            .collect()
    }

    /// Poll once and return any events since the previous poll.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let refs = self.discover();
        // Prune entries for sessions that vanished from discovery (deleted/pruned transcripts),
        // so a long-lived watcher (cvd) doesn't grow `seen` without bound. The size check skips
        // building the key set on the common nothing-vanished poll.
        if self.seen.len() > refs.len() {
            let live: std::collections::HashSet<String> = refs.iter().map(Self::key).collect();
            self.seen.retain(|k, _| live.contains(k));
        }
        for r in refs {
            let key = Self::key(&r);
            let trigger = Self::trigger_of(&r);
            match self.seen.get(&key) {
                None => {
                    let Some((total, messages)) = tail(&r, 0, usize::MAX) else {
                        continue;
                    };
                    let mark = Mark::Messages(total);
                    events.push(SessionEvent {
                        kind: EventKind::New,
                        reference: r,
                        new_messages: messages,
                    });
                    self.seen.insert(key, State { trigger, mark });
                }
                Some(state) if state.trigger != trigger => {
                    let Some(prev_len) = resolve(&r, state.mark) else {
                        continue;
                    };
                    let Some((total, new_messages)) = tail(&r, prev_len, usize::MAX) else {
                        continue;
                    };
                    let mark = Mark::Messages(total);
                    if !new_messages.is_empty() {
                        events.push(SessionEvent {
                            kind: EventKind::Updated,
                            reference: r,
                            new_messages,
                        });
                    }
                    self.seen.insert(key, State { trigger, mark });
                }
                Some(_) => {}
            }
        }
        events
    }

    /// Poll forever, invoking `on_event` for each event. Blocks the calling thread.
    pub fn run<F: FnMut(SessionEvent)>(&mut self, interval: Duration, mut on_event: F) -> ! {
        loop {
            for ev in self.poll() {
                on_event(ev);
            }
            std::thread::sleep(interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sref(harness: Harness, path: std::path::PathBuf) -> SessionRef {
        SessionRef {
            id: "baseline-test".into(),
            harness,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    /// The codex shapes that make a prefix count subtle: `has_events` decided from the head, an
    /// `event_msg` twin that dedups its `response_item`, and an assistant message held back until a
    /// trailing `token_count` attaches usage.
    const CODEX: &str = concat!(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"x","cwd":"/work"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working on it"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3}}}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
        "\n",
    );

    /// A byte baseline resolved after the file grew must equal what a full parse returned when the
    /// file was that long — at EVERY cut point, including mid-line (a record still being written).
    #[test]
    fn byte_baseline_resolves_to_the_count_a_parse_saw_then() {
        let dir = std::env::temp_dir().join(format!("cv-watch-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");
        let mut cases: Vec<(Harness, Vec<u8>)> = std::fs::read_dir(fixtures)
            .unwrap()
            .map(|e| (Harness::Claude, std::fs::read(e.unwrap().path()).unwrap()))
            .collect();
        cases.push((Harness::Codex, CODEX.as_bytes().to_vec()));

        for (h, full) in cases {
            let then = dir.join("then.jsonl");
            let now = dir.join("now.jsonl");
            std::fs::write(&now, &full).unwrap();
            let adapter = harness::for_harness(h).unwrap();
            let total = adapter.parse(&sref(h, now.clone())).unwrap().messages.len();
            assert!(total > 1, "{h} fixture must have several messages");
            for cut in 0..=full.len() {
                std::fs::write(&then, &full[..cut]).unwrap();
                let Ok(parsed) = adapter.parse(&sref(h, then.clone())) else {
                    continue; // the old baseline recorded nothing for an unparseable file either
                };
                assert_eq!(
                    resolve(&sref(h, now.clone()), Mark::Bytes(cut as u64)),
                    Some(parsed.messages.len()),
                    "{h} cut at byte {cut}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The baseline count and the appended tail are streamed instead of taken from a held parse;
    /// both must be exactly the parse's.
    #[test]
    fn streamed_count_and_tail_equal_the_parse() {
        let fx = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let mut checked = 0;
        for (h, rel) in [
            (Harness::Claude, "claude/rich_blocks.jsonl"),
            (Harness::Claude, "claude/sidechain_subagent.jsonl"),
            (Harness::Claude, "claude/compact_summary.jsonl"),
            (Harness::Claude, "claude/system_lines.jsonl"),
            (Harness::Gemini, "gemini/session_0_46_session_context.jsonl"),
            (Harness::Gemini, "gemini/checkpoint_legacy_array.json"),
            (Harness::OpenClaw, "openclaw/v3-full.jsonl"),
            (Harness::OpenClaw, "openclaw/acp-session.jsonl"),
            (Harness::LmStudio, "lmstudio/sample.conversation.json"),
            (Harness::LmStudio, "lmstudio/harmony.conversation.json"),
            (Harness::Cline, "cline/api_conversation_history.json"),
            (Harness::Roo, "roo/api_conversation_history.json"),
        ] {
            let r = sref(h, std::path::Path::new(fx).join(rel));
            let Ok(parsed) = harness::for_harness(h).unwrap().parse(&r) else {
                continue;
            };
            if !append_only(&r) {
                assert_eq!(baseline(&r), Some(Mark::Messages(parsed.messages.len())), "{h} {rel}");
            }
            // The streamed tail is the parse's tail, message for message, at every split.
            let n = parsed.messages.len();
            for skip in [0, 1, n / 2, n.saturating_sub(1), n, n + 3] {
                for keep in [0, 1, usize::MAX] {
                    let (total, got) = tail(&r, skip, keep).unwrap();
                    assert_eq!(total, n, "{h} {rel}");
                    let want: Vec<&Message> = parsed.messages.iter().skip(skip).take(keep).collect();
                    assert_eq!(
                        serde_json::to_value(&got).unwrap(),
                        serde_json::to_value(&want).unwrap(),
                        "{h} {rel} skip={skip} keep={keep}"
                    );
                }
            }
            checked += 1;
        }
        assert!(checked >= 8, "only {checked} fixtures parsed standalone");
    }

    /// A transcript REWRITTEN under a byte mark (truncated, or replaced by different content) is
    /// not an append, so no count can recover "what was seen then". What must hold is that the mark
    /// still resolves to something the new file can index: past the new end it resolves to the
    /// whole file (nothing re-emitted, nothing invented), and inside a replaced prefix it resolves
    /// to that prefix's own count. Neither Claude's `/compact` (it appends) nor `cv prune` (it
    /// writes a new session) rewrites in place; this pins the failure mode should anything else.
    #[test]
    fn a_byte_mark_on_a_rewritten_transcript_resolves_within_the_new_file() {
        let dir = std::env::temp_dir().join(format!("cv-watch-rewrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        for h in [Harness::Claude, Harness::Codex] {
            let full: Vec<u8> = if h == Harness::Codex {
                CODEX.as_bytes().to_vec()
            } else {
                std::fs::read(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/claude/rich_blocks.jsonl"
                ))
                .unwrap()
            };
            let r = sref(h, path.clone());
            let adapter = harness::for_harness(h).unwrap();
            let mark = Mark::Bytes(full.len() as u64);

            // Truncated to its first half (cut on a line boundary).
            let half = full[..full.len() / 2].iter().rposition(|b| *b == b'\n').unwrap() + 1;
            std::fs::write(&path, &full[..half]).unwrap();
            let now = adapter.parse(&r).unwrap().messages.len();
            assert_eq!(
                resolve(&r, mark),
                Some(now),
                "{h}: a mark past the end is the whole file"
            );
            let (total, fresh) = tail(&r, now, usize::MAX).unwrap();
            assert_eq!((total, fresh.len()), (now, 0), "{h}: nothing re-emitted");

            // Replaced: the old file's length now cuts through different records.
            let mut other = full[half..].to_vec();
            other.extend_from_slice(&full);
            std::fs::write(&path, &other).unwrap();
            let total = adapter.parse(&r).unwrap().messages.len();
            let seen = resolve(&r, mark).unwrap();
            std::fs::write(dir.join("prefix.jsonl"), &other[..full.len()]).unwrap();
            let prefix = adapter
                .parse(&sref(h, dir.join("prefix.jsonl")))
                .unwrap()
                .messages
                .len();
            assert_eq!(seen, prefix, "{h}: a mark inside a rewrite is that prefix's count");
            assert!(seen <= total, "{h}");
            assert_eq!(tail(&r, seen, usize::MAX).unwrap().1.len(), total - seen, "{h}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn baseline_is_a_byte_mark_only_for_append_only_transcripts() {
        let dir = std::env::temp_dir().join(format!("cv-watch-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("s.jsonl");
        std::fs::write(&jsonl, CODEX).unwrap();
        let len = CODEX.len() as u64;
        assert_eq!(baseline(&sref(Harness::Claude, jsonl.clone())), Some(Mark::Bytes(len)));
        assert_eq!(baseline(&sref(Harness::Codex, jsonl.clone())), Some(Mark::Bytes(len)));
        // A compressed rollout's byte prefix is not a transcript prefix: parse it instead.
        assert!(!append_only(&sref(Harness::Codex, dir.join("r.jsonl.zst"))));
        // Missing file: no mark, as before.
        assert_eq!(baseline(&sref(Harness::Gemini, dir.join("absent.json"))), None);
        assert_eq!(resolve(&sref(Harness::Claude, jsonl), Mark::Messages(3)), Some(3));
        std::fs::remove_dir_all(&dir).ok();
    }
}
