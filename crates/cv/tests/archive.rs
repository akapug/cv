//! Integration tests for `cv archive`: `sync` idempotency against a fixture corpus, and `ls` /
//! `path` on an empty archive — hermetic via a temp `$HOME` + `$CLUSTERVISION_HOME` passed only
//! to child processes.

mod common;

use common::world;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn count_lines(p: &Path) -> usize {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[test]
fn sync_is_idempotent() {
    let w = world("sync");

    // First sync archives both sessions.
    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        out.contains("2 archived, 0 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    assert!(w.cv_home.join("archive/claude/alphasess.json").is_file());
    assert!(w.cv_home.join("archive/claude/betasess.json").is_file());
    assert_eq!(count_lines(&w.cv_home.join("catalog.jsonl")), 2);

    // Second sync: nothing changed → nothing rewritten, no duplicate catalog rows.
    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        out.contains("0 archived, 2 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    assert_eq!(count_lines(&w.cv_home.join("catalog.jsonl")), 2, "no dupes on resync");

    // ls shows each session exactly once.
    let (out, _) = w.cv_ok(&["archive", "ls"]);
    assert!(out.contains("2 session(s)"), "{out}");
    assert_eq!(out.matches("alphases").count(), 1, "{out}");
    assert!(out.contains("alpha adventures"), "{out}");

    // Append a turn to one session: only that one re-archives; the catalog dedupes by key.
    let mut body = fs::read_to_string(w.transcript("alphasess")).unwrap();
    body.push_str(
        &json!({"type": "user", "uuid": "u9", "timestamp": "2026-06-02T10:00:00Z",
                "message": {"role": "user", "content": "one more thing"}})
        .to_string(),
    );
    body.push('\n');
    fs::write(w.transcript("alphasess"), body).unwrap();

    let (out, _) = w.cv_ok(&["archive", "sync"]);
    assert!(
        out.contains("1 archived, 1 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["archive", "ls"]);
    assert!(
        out.contains("2 session(s)"),
        "changed session must not duplicate:\n{out}"
    );
    assert!(out.contains("3 msgs"), "updated message count visible:\n{out}");

    // The archived JSON is the parsed IR, scrubbed of nothing — spot-check its shape.
    let archived: Value =
        serde_json::from_str(&fs::read_to_string(w.cv_home.join("archive/claude/alphasess.json")).unwrap())
            .expect("archived session is valid JSON");
    assert_eq!(archived["id"], "alphasess");
    assert_eq!(archived["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn ls_and_path_on_empty_archive() {
    let w = world("empty");
    let (out, _) = w.cv_ok(&["archive", "ls"]);
    assert!(out.contains("archive empty"), "{out}");
    let (out, _) = w.cv_ok(&["archive", "path"]);
    assert_eq!(out.trim(), w.cv_home.display().to_string(), "{out}");
}
