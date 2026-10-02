//! An opencode baseline must not read a session to count it (task/4088).
//!
//! OpenCode keeps its sessions in SQLite rather than in append-only files, so
//! PR #18's byte mark does not apply and `watch::baseline` falls through to
//! `count()`, which streams the WHOLE session just to learn how many messages
//! it holds. Measured by the task/4063 builder: with 213 opencode sessions the
//! baseline peaked at 587 MB, against 18 MB for codex and 22 MB for claude —
//! the broad-filter peak PR #18 set out to remove, still there, entirely in
//! this one adapter.
//!
//! THE OBVIOUS CURE IS WRONG, and the second arm below proves it rather than
//! asserting it. The task proposed "count messages per session with a SQL
//! COUNT". A message exists in the database whether or not it maps to an IR
//! message: `build_messages` returns nothing for a record whose parts
//! contribute no content AND whose bag holds no extra fact AND whose kind is
//! the default for its role. Measured on that shape, the SQL count says 5
//! where a parse says 4. A mark that disagrees with the parse loses or
//! repeats messages — the exact failure `Mark` exists to prevent — so the
//! count has to match the SURVIVAL RULE, not the row count.
#![cfg(feature = "sqlite")]

use cv_core::harness::opencode::OpenCode;
use cv_core::ir::SessionRef;
use cv_core::watch::{baseline, Mark};
use cv_core::Adapter;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// A real `opencode.db`: `sessions` sessions of `per_session` user and
/// assistant messages, each with the text part a live store carries.
fn plant_db(dir: &Path, sessions: usize, per_session: usize) -> PathBuf {
    let path = dir.join("opencode.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL,
            directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL
         );
         CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );
         CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );",
    )
    .unwrap();
    for s in 0..sessions {
        let sid = format!("ses_{s:03}");
        conn.execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, \
             time_created, time_updated) VALUES (?1,'p','s','/tmp','t','1',1,1)",
            [&sid],
        )
        .unwrap();
        for m in 0..per_session {
            for role in ["user", "assistant"] {
                let mid = format!("{sid}_m{m}_{role}");
                conn.execute(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data) \
                     VALUES (?1, ?2, 1, 1, ?3)",
                    rusqlite::params![mid, sid, format!(r#"{{"role":"{role}"}}"#)],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                     VALUES (?1, ?2, ?3, 1, 1, ?4)",
                    rusqlite::params![
                        format!("{mid}_p"),
                        mid,
                        sid,
                        format!(r#"{{"type":"text","text":"body {m}"}}"#)
                    ],
                )
                .unwrap();
            }
        }
    }
    path
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cv-4088-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn refs(db: &Path) -> (OpenCode, Vec<SessionRef>) {
    let adapter = OpenCode::with_db(db.to_path_buf());
    let r = adapter.discover().expect("discover the planted db");
    assert!(!r.is_empty(), "the planted db must be discovered");
    (adapter, r)
}

/// THE EQUIVALENCE: whatever the mark stands for must be the count a PARSE
/// gives, or a follower loses or repeats messages. This is where a plain
/// `COUNT(*)` fails — see the module doc, and the arm below builds the shape
/// that shows it.
#[test]
fn an_opencode_baseline_mark_equals_the_parsed_count() {
    let dir = tmp("count");
    let (adapter, rs) = refs(&plant_db(&dir, 3, 3));
    for r in rs {
        let parsed = adapter.parse(&r).expect("parse").messages.len();
        assert_eq!(parsed, 6, "3 turns x 2 roles");
        assert_eq!(
            baseline(&r),
            Some(Mark::Messages(parsed)),
            "a baseline mark must stand for the count a full parse gives"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// THE SHAPE THAT BREAKS `COUNT(*)`: a message record that maps to no IR
/// message. A naive SQL count includes it and the mark then over-counts by
/// one, so the follower skips a message that was never reported.
#[test]
fn a_message_that_maps_to_nothing_is_not_counted() {
    let dir = tmp("ghost");
    let db = plant_db(&dir, 1, 2);
    let conn = Connection::open(&db).unwrap();
    // No parts, no `agent`/`finish`/`error`-style extra, default kind for its
    // role: the shape `build_messages` drops.
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('ghost','ses_000',1,1,'{\"role\":\"user\"}')",
        [],
    )
    .unwrap();
    drop(conn);

    let (adapter, rs) = refs(&db);
    let r = &rs[0];
    let parsed = adapter.parse(r).expect("parse").messages.len();
    assert_eq!(parsed, 4, "2 turns x 2 roles; the ghost maps to nothing");
    assert_eq!(
        baseline(r),
        Some(Mark::Messages(parsed)),
        "the mark must count what a parse counts, not what the table holds"
    );
    std::fs::remove_dir_all(&dir).ok();
}
