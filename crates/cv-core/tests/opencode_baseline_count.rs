//! An opencode baseline mark must be the count a parse gives (task/4088).
//!
//! OpenCode keeps its sessions in SQLite rather than in append-only files, so
//! PR #18's byte mark does not apply and `watch::baseline` takes an eager
//! count. Whatever that count is, it has to equal the number of IR messages a
//! parse builds, or a follower loses or repeats messages, the failure `Mark`
//! exists to prevent.
//!
//! A database row count is NOT that number, in both directions. A record that
//! maps to nothing (no content, no extra fact, the default kind for its role)
//! is a row the parse drops (the second arm). A tool part that carries a result
//! is two IR messages from one row, and a record with no content survives on
//! its bag or its kind (the third arm). A SQL query that tried to restate the
//! mapping's survival rule undercounted 183 of 328 sessions in a real store, so
//! the count is taken through the mapping itself.
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

/// Every record shape `build_messages` maps, against the mark. The two arms above hold one text
/// part per message, so they never reach the shapes where the row count and the parse part ways
/// in the other direction: a tool part that carries a result is TWO IR messages (the call, and a
/// trailing `Role::Tool` result), and a record with no content still survives on its bag
/// (step/patch parts), its kind (`summary: true`, a `subtask`) or an inline summary body. On
/// ember's 328-session `opencode.db` a hand-written survival query undercounted 183 of them by
/// 6,787 messages in all, and an undercounted mark re-emits that many old messages as new.
#[test]
fn the_mark_counts_what_the_parse_builds_for_every_record_shape() {
    let dir = tmp("shapes");
    let db = plant_db(&dir, 1, 0);
    let conn = Connection::open(&db).unwrap();
    let mut t = 10;
    let mut message = |mid: &str, data: &str, parts: &[&str]| {
        t += 1;
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1,'ses_000',?2,?2,?3)",
            rusqlite::params![mid, t, data],
        )
        .unwrap();
        for (i, p) in parts.iter().enumerate() {
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, 'ses_000', ?3, ?3, ?4)",
                rusqlite::params![format!("{mid}_p{i}"), mid, t, p],
            )
            .unwrap();
        }
    };
    // 1: an ordinary prompt.
    message("m01", r#"{"role":"user"}"#, &[r#"{"type":"text","text":"run it"}"#]);
    // 2: a completed tool call is the call AND its result.
    message(
        "m02",
        r#"{"role":"assistant"}"#,
        &[
            r#"{"type":"step-start"}"#,
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{},"output":"ok"}}"#,
            r#"{"type":"step-finish"}"#,
        ],
    );
    // 2: a failed tool call too.
    message(
        "m03",
        r#"{"role":"assistant"}"#,
        &[r#"{"type":"tool","callID":"c2","tool":"bash","state":{"status":"error","input":{},"error":"boom"}}"#],
    );
    // 1: no content, but the step parts are kept in the bag.
    message(
        "m04",
        r#"{"role":"assistant"}"#,
        &[r#"{"type":"step-start"}"#, r#"{"type":"step-finish"}"#],
    );
    // 1: a compaction summary marker with no parts.
    message("m05", r#"{"role":"assistant","summary":true}"#, &[]);
    // 1: an older inline summary body with no parts.
    message("m06", r#"{"role":"user","summary":{"body":"what came before"}}"#, &[]);
    // 1: a subtask spawn.
    message(
        "m07",
        r#"{"role":"user"}"#,
        &[r#"{"type":"subtask","agent":"general","prompt":"look"}"#],
    );
    // 0: reasoning with metadata but neither text nor a signature contributes nothing.
    message(
        "m08",
        r#"{"role":"assistant"}"#,
        &[r#"{"type":"reasoning","text":"","metadata":{"openai":{"itemId":"r1"}}}"#],
    );
    drop(conn);

    let (adapter, rs) = refs(&db);
    let r = &rs[0];
    let parsed = adapter.parse(r).expect("parse").messages.len();
    assert_eq!(parsed, 9, "1 + 2 + 2 + 1 + 1 + 1 + 1 + 0");
    assert_eq!(
        baseline(r),
        Some(Mark::Messages(parsed)),
        "the mark must count what a parse builds"
    );
    std::fs::remove_dir_all(&dir).ok();
}
