//! The source of a `--fork-session` session. Claude Code copies the source's active chain into a
//! new file under the new `sessionId` and writes no pointer back; the copied records keep their
//! `uuid`s and timestamps, so the shared `uuid` is the link.
//!
//! A fork is recognized from its own head, so an ordinary session pays one short head read:
//!
//! - its first `uuid` record is older than a record written before it (the fork's own
//!   `queue-operation` for the prompt it was started with precedes the copied history), or
//! - it begins at a `compact_boundary` (the copy starts at the source's last compaction, so the
//!   source holds that boundary mid-file).
//!
//! Its source is a sibling transcript in the same project directory that was live at the copied
//! record's time (modified since, first record no later), and either begins with the same `uuid`
//! without itself looking copied, or holds the boundary `uuid` past its own first record. Only
//! those few candidates are read whole. Siblings that are themselves forks of the same source are
//! never chosen; a fork of a fork resolves to the original.
//! Out-of-order head timestamps can falsely suggest a fork, costing a sibling-directory scan,
//! candidate head reads, and whole-file searches for candidates requiring a boundary match.

use super::record::Facts;
use crate::harness::store::jsonl;
use crate::stream::Flow;
use chrono::{DateTime, Utc};
#[cfg(test)]
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// How far a head read goes before giving up on finding the first `uuid` record.
const HEAD_LINES: usize = 64;

/// What a transcript's head says: its first `uuid` record and whether it looks copied.
struct Head {
    uuid: String,
    at: Option<DateTime<Utc>>,
    /// The first record is a `compact_boundary`.
    boundary: bool,
    /// The head looks copied (see the module docs).
    copied: bool,
    /// The newest time written before the first `uuid` record: in a fork, when the fork began, so
    /// every record older than it was copied.
    began: Option<DateTime<Utc>>,
}

/// The head through the record codec's byte source and its typed [`Facts`]: at most
/// [`HEAD_LINES`] physical lines, counting blank and rejected ones.
fn head(path: &Path) -> Option<Head> {
    let read = jsonl::Read::open(path).ok()?;
    let mut newest: Option<DateTime<Utc>> = None;
    let mut found = None;
    let mut lines = 0;
    read.lines(0, |line| {
        lines += 1;
        if let Some(view) = line.view {
            let (facts, _) = Facts::parse(view)?;
            let at = facts.timestamp.as_deref().and_then(super::parse_ts);
            if let Some(uuid) = facts.uuid {
                // A head boundary has no string parent, as decoded by `Facts::parent`.
                let boundary = facts.subtype.as_deref() == Some("compact_boundary") && facts.parent.flatten().is_none();
                let older = matches!((at, newest), (Some(a), Some(n)) if a < n);
                found = Some(Head {
                    uuid,
                    at,
                    boundary,
                    copied: boundary || older,
                    began: newest,
                });
                return Ok(Flow::Stop);
            }
            if let Some(a) = at {
                newest = Some(newest.map_or(a, |n| n.max(a)));
            }
        }
        Ok(if lines == HEAD_LINES {
            Flow::Stop
        } else {
            Flow::Continue
        })
    })
    .ok()?;
    found
}

fn modified(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path).ok()?.modified().ok().map(DateTime::<Utc>::from)
}

/// Whether `path` holds `needle` anywhere, over bounded windows that keep the needle's length
/// minus one byte between reads.
fn holds(path: &Path, needle: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    if needle.is_empty() {
        return true;
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut window = Vec::new();
    loop {
        let Ok(bytes) = reader.fill_buf() else {
            return false;
        };
        if bytes.is_empty() {
            return false;
        }
        window.extend_from_slice(bytes);
        if memchr::memmem::find(&window, needle.as_bytes()).is_some() {
            return true;
        }
        let n = bytes.len();
        reader.consume(n);
        window.drain(..window.len() - (needle.len() - 1).min(window.len()));
    }
}

/// When the fork at `path` began, so its records written earlier are its source's copied turns;
/// `None` unless a source is found ([`source_tracked`]).
pub(super) fn copied_before(path: &Path, sink: &mut dyn crate::MessageSink) -> Option<DateTime<Utc>> {
    source_tracked(path, sink)?;
    head(path)?.began
}

/// The session id `path` was forked from, when its head says it is a fork and a sibling holds the
/// records it copied.
#[cfg(test)]
pub(super) fn source(path: &Path) -> Option<String> {
    source_tracked(path, &mut crate::stream::Discard)
}

pub(super) fn source_tracked(path: &Path, sink: &mut dyn crate::MessageSink) -> Option<String> {
    let me = head(path)?;
    if !me.copied {
        return None;
    }
    let at = me.at?;
    let dir = path.parent()?;
    sink.dependency(dir); // A new/deleted eligible sibling can change the inferred source.
    let mut candidates: Vec<(DateTime<Utc>, PathBuf, Head)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != path)
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".jsonl") && !n.ends_with(".flat.jsonl"))
        })
        .filter(|p| {
            sink.dependency(p);
            p.is_file()
        })
        // Live at the copied record's time: written since.
        .filter(|p| modified(p).is_some_and(|m| m >= at))
        .filter_map(|p| {
            let h = head(&p)?;
            let first = h.at?;
            (first <= at).then_some((first, p, h))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .find(|(_, p, h)| {
            if h.uuid == me.uuid {
                !h.copied
            } else {
                me.boundary && holds(p, &me.uuid)
            }
        })
        .and_then(|(_, p, _)| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, lines: &[Value]) -> PathBuf {
        let p = dir.join(name);
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(&p, text.join("\n") + "\n").unwrap();
        p
    }

    fn rec(uuid: &str, parent: Option<&str>, ts: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "timestamp": ts,
               "message": {"role": "user", "content": "hi"}})
    }

    fn queued(ts: &str) -> Value {
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": ts})
    }

    #[test]
    fn projected_heads_match_value_validation_and_physical_line_limits() {
        let w = crate::test_world("fork-head-projection");
        let path = w.base.join("head.jsonl");
        let uuid = r#"{"uuid":"shared","timestamp":"2026-09-19T00:00:00Z"}"#;
        let queued = r#"{"timestamp":"2026-09-19T01:00:00Z"}"#;
        let mut cases: Vec<Vec<u8>> = vec![
            format!("{queued}\n{uuid}\n").into_bytes(),
            format!("{}{uuid}\n", "\n".repeat(63)).into_bytes(),
            format!("{}{uuid}\n", "\n".repeat(64)).into_bytes(),
            format!("malformed\nnull\n[1,2]\n{uuid}\n").into_bytes(),
            format!("\u{a0}{queued}\n{uuid}\n").into_bytes(),
            format!("{{\"uuid\":\"bad\",\"ignored\":1e400}}\n{uuid}\n").into_bytes(),
            format!("{{\"uuid\":\"bad\",\"ignored\":\"\\ud800\"}}\n{uuid}\n").into_bytes(),
            br#"{"uuid":"first","uuid":17}
{"uuid":"last","uuid":"final","subtype":"compact_boundary","parentUuid":null}
"#
            .to_vec(),
            br#"{"uuid":"parent","subtype":"compact_boundary","parentUuid":false}
"#
            .to_vec(),
            br#"{"uuid":"escaped\u0031","timestamp":[],"parentUuid":null}
"#
            .to_vec(),
        ];
        cases.push(format!("{{\"uuid\":\"giant\",\"message\":\"{}\"}}\n", "payload".repeat(150_000)).into_bytes());
        cases.push(
            format!(
                "{{\"uuid\":\"escaped\",\"message\":\"{}\"}}\n",
                "\\u0061".repeat(150_000)
            )
            .into_bytes(),
        );
        // Frozen from the retired line/`Value` head reader (decision 11: goldens, not oracles).
        let mut frozen = crate::emit::frozen::Frozen::new("claude-fork-heads");
        for (i, bytes) in cases.iter().enumerate() {
            fs::write(&path, bytes).unwrap();
            let observed = head(&path).map(|h| (h.uuid, h.at, h.boundary, h.copied, h.began));
            frozen.insert(format!("case {i}"), serde_json::to_value(observed).unwrap());
        }
        frozen.check().unwrap();
        // An invalid UTF-8 line is one rejected record, not the end of the file.
        fs::write(&path, [b"\xff\n".as_slice(), uuid.as_bytes()].concat()).unwrap();
        assert_eq!(head(&path).map(|h| h.uuid).as_deref(), Some("shared"));
        fs::write(&path, [b"prefix\n\xff\n".as_slice(), b"needle\n"].concat()).unwrap();
        assert!(holds(&path, "needle"));
    }

    /// An uncompacted source: the fork starts with the same `uuid`, after its own newer queue
    /// record. The source, a sibling fork, and an unrelated session are told apart.
    #[test]
    fn a_fork_of_an_uncompacted_session_names_its_source() {
        let dir = std::env::temp_dir().join(format!("cv-claude-fork-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let t0 = "2026-10-11T20:00:00Z";
        let src = write(
            &dir,
            "src.jsonl",
            &[
                queued(t0),
                rec("u0", None, t0),
                rec("u1", Some("u0"), "2026-10-11T20:00:05Z"),
            ],
        );
        let fork_lines = [
            json!({"type": "custom-title", "customTitle": "t"}),
            queued("2026-10-11T21:00:00Z"),
            rec("u0", None, t0),
            rec("u1", Some("u0"), "2026-10-11T20:00:05Z"),
            rec("f1", Some("u1"), "2026-10-11T21:00:01Z"),
        ];
        let fork = write(&dir, "fork.jsonl", &fork_lines);
        let sibling = write(&dir, "sibling.jsonl", &fork_lines);
        write(&dir, "other.jsonl", &[queued(t0), rec("o0", None, t0)]);
        assert_eq!(source(&fork).as_deref(), Some("src"));
        assert_eq!(source(&sibling).as_deref(), Some("src"));
        assert_eq!(source(&src), None, "the source's head is not a copy");
        let _ = fs::remove_dir_all(&dir);
    }
}
