//! Integration tests for the catalog read path ([`cv_core::sessions`]) and its freshness probe:
//! cold-catalog transparency, new-file / appended-file / vanished-dir detection, and the
//! staleness-backstop escape hatch.
//!
//! Each test reads its own world's `HOME` and `CLUSTERVISION_HOME` in-process
//! (`cv_testkit::World::in_process`). They also `sleep` past the probe's 2s watch-fudge window
//! where a test needs recorded (non-zero) watch mtimes to be the thing that detects a change.

use cv_core::ir::{Harness, SessionRef};
use cv_testkit::World;
use std::fs;
use std::path::PathBuf;

/// How long the recorded-watch fudge window is (catalog's WATCH_FUDGE = 2s) plus margin.
const FUDGE: std::time::Duration = std::time::Duration::from_millis(2100);

/// Write a claude-format session of `n` user messages under project dir `proj`.
fn write_session(w: &World, proj: &str, name: &str, n: usize) -> PathBuf {
    let dir = w.home.join(".claude/projects").join(proj);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.jsonl"));
    let body: String = (0..n).map(|i| user_line(name, i)).collect();
    fs::write(&path, body).unwrap();
    path
}

fn user_line(sid: &str, i: usize) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "user", "uuid": format!("{sid}-u{i}"), "sessionId": sid,
            "timestamp": format!("2026-01-0{}T10:0{}:00Z", (i % 9) + 1, i % 10),
            "cwd": "/work/proj",
            "message": {"role": "user", "content": format!("message {i} of {sid}")}
        })
    )
}

/// The comparable identity of a ref (timestamps excluded: the catalog stores second precision).
fn key(r: &SessionRef) -> (String, String, PathBuf, Option<usize>) {
    (r.harness.as_str().into(), r.id.clone(), r.path.clone(), r.message_count)
}

fn keys(refs: &[SessionRef]) -> Vec<(String, String, PathBuf, Option<usize>)> {
    let mut v: Vec<_> = refs.iter().map(key).collect();
    v.sort();
    v
}

fn ids(refs: &[SessionRef]) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().map(|r| r.id.clone()).collect();
    v.sort();
    v
}

/// A cold catalog must transparently full-discover (same answer as `discover_all`), and an
/// immediately-following warm read must agree exactly.
#[test]
fn cold_catalog_matches_discover_all() {
    let w = World::in_process("cold", cv_core::env::Env::enter_isolated);
    write_session(&w, "-work-proj", "alphasess", 2);
    write_session(&w, "-work-proj", "betasess", 3);

    let cold = cv_core::sessions(); // cold: no last_full_sync stamp yet
    assert_eq!(ids(&cold), vec!["alphasess", "betasess"], "cold read = full discovery");

    let direct = cv_core::discover_all();
    let warm = cv_core::sessions();
    assert_eq!(keys(&warm), keys(&direct), "warm catalog read == discover_all");
    assert_eq!(warm.iter().map(key).count(), 2);
    let alpha = warm.iter().find(|r| r.id == "alphasess").unwrap();
    assert_eq!(alpha.harness, Harness::Claude);
    assert_eq!(alpha.message_count, Some(2));
    assert_eq!(alpha.cwd.as_deref(), Some(std::path::Path::new("/work/proj")));
}

/// A new session file in an already-watched directory changes that dir's mtime → probe detects it.
/// A new *project directory* changes the root's mtime → also detected.
#[test]
fn new_session_files_detected() {
    let w = World::in_process("new", cv_core::env::Env::enter_isolated);
    write_session(&w, "-work-proj", "alphasess", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE); // age past the watch fudge…
    cv_core::sessions(); // …and re-record real (non-zero) watch mtimes

    write_session(&w, "-work-proj", "betasess", 1); // new file in a watched dir
    write_session(&w, "-work-other", "gammasess", 1); // new dir under the watched root
    let refs = cv_core::sessions();
    assert_eq!(ids(&refs), vec!["alphasess", "betasess", "gammasess"]);
}

/// An in-place append doesn't change any directory mtime; the all-primary-file check
/// (against the scan cache's recorded `(mtime, size)`) must catch it.
#[test]
fn append_in_top_k_detected() {
    let w = World::in_process("append", cv_core::env::Env::enter_isolated);
    let path = write_session(&w, "-work-proj", "alphasess", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions(); // stabilize watches

    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(user_line("alphasess", 2).as_bytes()).unwrap();
    drop(f);

    let refs = cv_core::sessions();
    let alpha = refs.iter().find(|r| r.id == "alphasess").expect("alphasess present");
    assert_eq!(
        alpha.message_count,
        Some(3),
        "append must be re-scanned via the primary probe"
    );
}

/// Deleting a session file (dir mtime changes) or a whole project dir (watched path vanishes)
/// must drop those sessions from the read path.
#[test]
fn vanished_sessions_dropped() {
    let w = World::in_process("vanish", cv_core::env::Env::enter_isolated);
    let alpha_path = write_session(&w, "-work-proj", "alphasess", 2);
    write_session(&w, "-work-other", "gammasess", 1);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions();

    fs::remove_file(&alpha_path).unwrap(); // unlink bumps the parent dir's mtime
    let refs = cv_core::sessions();
    assert_eq!(ids(&refs), vec!["gammasess"], "deleted session must not appear");

    std::thread::sleep(FUDGE);
    cv_core::sessions();
    fs::remove_dir_all(w.home.join(".claude/projects/-work-other")).unwrap(); // watched dir itself vanishes
    let refs = cv_core::sessions();
    assert_eq!(
        ids(&refs),
        Vec::<String>::new(),
        "vanished dir's sessions must not appear"
    );
}

/// The warm path really reads the catalog (a row only the catalog knows shows up), and forcing a
/// full discovery — what `cv ls --fresh` and `CLUSTERVISION_MAX_STALE_SECS=0` do — heals it.
#[test]
fn warm_path_reads_catalog_and_full_discovery_heals() {
    let w = World::in_process("warm", cv_core::env::Env::enter_isolated);
    let alpha = write_session(&w, "-work-proj", "alphasess", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions();

    // Plant an alias row over an already captured source. Nothing on disk changes, so the
    // probe stays clean — only an actual catalog read can return the extra ID.
    let phantom = SessionRef {
        id: "phantom-row".into(),
        harness: Harness::Claude,
        path: alpha,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: Some(7),
    };
    cv_core::catalog::sync(std::slice::from_ref(&phantom));
    let refs = cv_core::sessions();
    assert!(
        refs.iter().any(|r| r.id == "phantom-row"),
        "warm sessions() must read the catalog, not re-scan: {:?}",
        ids(&refs)
    );

    // Staleness backstop at 0 = every read is a full discovery; the replace-sync drops the phantom.
    w.scope.set("CLUSTERVISION_MAX_STALE_SECS", "0");
    let refs = cv_core::sessions();
    w.scope.remove("CLUSTERVISION_MAX_STALE_SECS");
    assert_eq!(ids(&refs), vec!["alphasess"], "full discovery heals phantom rows");

    // And discover_all() itself (the `--fresh` path) returns only what's on disk.
    cv_core::catalog::sync(std::slice::from_ref(&phantom));
    let refs = cv_core::discover_all();
    assert_eq!(ids(&refs), vec!["alphasess"]);
}

/// A Codex rollout over the discovery scan cap lists with `message_count` `None` ("not counted": the
/// middle was never read). The first full parse writes the true count back, and the next discovery
/// keeps it (a scan-cache hit while the file is unchanged), so `cv ls` shows the real number.
#[test]
fn a_full_parse_fills_a_not_counted_catalog_row() {
    let w = World::in_process("fill", cv_core::env::Env::enter_isolated);

    let dir = w.home.join(".codex/sessions/2026/01/01");
    fs::create_dir_all(&dir).unwrap();
    let id = "019a0000-0000-7000-8000-00000000f111";
    let msg = |ts: &str, role: &str, kind: &str, text: &str| {
        serde_json::json!({"timestamp": ts, "type": "response_item",
            "payload": {"type": "message", "role": role, "content": [{"type": kind, "text": text}]}})
        .to_string()
    };
    let mut lines = vec![
        serde_json::json!({"timestamp": "2026-01-01T10:00:00Z", "type": "session_meta",
            "payload": {"id": id, "cwd": "/work/proj", "timestamp": "2026-01-01T10:00:00Z"}})
        .to_string(),
        msg("2026-01-01T10:00:01Z", "user", "input_text", "first"),
        msg("2026-01-01T10:00:02Z", "assistant", "output_text", "one"),
    ];
    // Nine MiB of reply in the middle: past the 8 MiB full-scan cap, out of the sampled head/tail.
    lines.push(msg(
        "2026-01-01T10:00:03Z",
        "assistant",
        "output_text",
        &"x".repeat(9 << 20),
    ));
    lines.push(msg("2026-01-01T10:00:04Z", "user", "input_text", "second"));
    lines.push(msg("2026-01-01T10:00:05Z", "assistant", "output_text", "two"));
    let path = dir.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    fs::write(&path, lines.join("\n") + "\n").unwrap();

    let row = |refs: Vec<SessionRef>| refs.into_iter().find(|r| r.id == id).expect("codex row listed");
    assert_eq!(
        row(cv_core::discover_all()).message_count,
        None,
        "over the cap: not counted"
    );
    assert_eq!(row(cv_core::sessions()).message_count, None);

    let (r, adapter) = cv_core::find(id, Some(Harness::Codex)).unwrap().expect("resolves");
    // A stream that stops early (`cv show --range 0..1`) read a prefix: it proves nothing.
    let mut head = |_m: cv_core::Message| cv_core::Flow::Stop;
    cv_core::stream::stream_counted(
        adapter.as_ref(),
        &r,
        &cv_core::Demand::LEAN.with_spans(),
        &mut head,
        cv_core::stream::Pass::RECORD,
    )
    .unwrap();
    assert_eq!(
        row(cv_core::sessions()).message_count,
        None,
        "a partial read fills nothing"
    );
    // A stream that reaches the end (`cv show`, `cv index`) fills the row with the parse's count.
    let mut all = |_m: cv_core::Message| cv_core::Flow::Continue;
    cv_core::stream::stream_counted(
        adapter.as_ref(),
        &r,
        &cv_core::Demand::LEAN.with_spans(),
        &mut all,
        cv_core::stream::Pass::RECORD,
    )
    .unwrap();
    assert_eq!(row(cv_core::sessions()).message_count, Some(5), "the streamed count");
    // `parse` (Codex's whole-text path) agrees, and a filled row is never rewritten.
    let parsed = adapter.parse(&r).unwrap();
    let turns = parsed.messages.iter().filter(|m| m.is_turn()).count();
    assert_eq!(
        turns,
        5,
        "{:#?}",
        parsed.messages.iter().map(|m| (m.role, m.kind)).collect::<Vec<_>>()
    );

    assert_eq!(
        row(cv_core::sessions()).message_count,
        Some(5),
        "the catalog row holds the parse's count"
    );
    assert_eq!(
        row(cv_core::discover_all()).message_count,
        Some(5),
        "and a re-discovery keeps it (scan-cache hit)"
    );
}

/// Why the follower (`watch::Watcher`, `cv watch`'s hot loop) takes `discover_all()` instead of the
/// Quiet sessions outside the former top-50 window refresh through the catalog too.
#[test]
fn quiet_session_append_refreshes_catalog_and_follower() {
    let w = World::in_process("follow", cv_core::env::Env::enter_isolated);
    for k in 0..50 {
        write_session(&w, "-work-proj", &format!("busy{k:02}"), 9); // updated 2026-01-09
    }
    let quiet = write_session(&w, "-work-proj", "quietsess", 1); // updated 2026-01-01: rank 51
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions();
    let mut watcher = cv_core::watch::Watcher::new(
        cv_core::watch::Filter {
            harness: Some(Harness::Claude),
            cwd_contains: None,
        },
        false,
    );

    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&quiet).unwrap();
    f.write_all(user_line("quietsess", 1).as_bytes()).unwrap();
    drop(f);

    let listed = cv_core::sessions();
    let q = listed.iter().find(|r| r.id == "quietsess").unwrap();
    assert_eq!(
        q.message_count,
        Some(2),
        "every primary path is checked, including quiet sessions"
    );

    let mut events = Vec::new();
    watcher.poll(cv_core::watch::Poll::default(), |ev| {
        let taken = ev.new_messages.len();
        events.push(ev);
        taken
    });
    let ev = events
        .iter()
        .find(|e| e.reference.id == "quietsess")
        .expect("the follower reports the append");
    assert_eq!(ev.kind, cv_core::watch::EventKind::Updated);
    assert_eq!(ev.new_messages.len(), 1);
}

/// `has:<affordance>` is a catalog-tier answer: a full read records the session's affordance
/// counts, `AffordanceTable` serves them while the file is unchanged, an append makes them stale
/// (and the next query's parse refreshes them), and a session never read is still answered, by a
/// parse. The query's answer is the same either way.
#[test]
fn a_full_read_counts_affordances_for_has() {
    let w = World::in_process("affordances", cv_core::env::Env::enter_isolated);
    let planner = write_session(&w, "proj", "planner", 1);
    let todo = serde_json::json!({
        "type": "assistant", "uuid": "planner-a", "sessionId": "planner", "timestamp": "2026-01-02T10:00:00Z",
        "cwd": "/work/proj",
        "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "TodoWrite",
            "input": {"todos": [{"content": "x", "status": "pending"}]}}]}
    });
    fs::write(
        &planner,
        fs::read_to_string(&planner).unwrap() + &todo.to_string() + "\n",
    )
    .unwrap();
    write_session(&w, "proj", "chatter", 2);
    let refs = cv_core::sessions();
    let by = |id: &str| refs.iter().find(|r| r.id == id).cloned().expect("listed");
    let (planner_ref, chatter_ref) = (by("planner"), by("chatter"));

    // Never read: nothing counted, and the query still answers (by parsing, which counts).
    let table = cv_core::catalog::AffordanceTable::load();
    assert!(table.counts(&planner_ref).is_none());
    let query = cv_core::query::SessionQuery::parse("has:plan").unwrap();
    assert!(query.needs_counted() && !query.needs_events());
    let run = || {
        let mut v = cv_core::sessions();
        cv_core::query::filter_refs(&mut v, &query, &cv_core::query::TextSets::empty()).unwrap();
        ids(&v)
    };
    assert_eq!(run(), ["planner"]);

    // Counted now, both ways round.
    let table = cv_core::catalog::AffordanceTable::load();
    let counts = table.counts(&planner_ref).expect("the query's parse counted it");
    assert_eq!(counts.get(&cv_core::affordance::Kind::Plan), Some(&1));
    assert_eq!(table.counts(&chatter_ref), Some(Default::default()));
    assert_eq!(run(), ["planner"], "the catalog answer is the parse answer");

    // An append (the file signature moves) makes the stored counts stale until re-read.
    fs::write(
        &planner,
        fs::read_to_string(&planner).unwrap() + &user_line("planner", 9),
    )
    .unwrap();
    assert!(cv_core::catalog::AffordanceTable::load().stale(&planner_ref));
    assert_eq!(run(), ["planner"]);
}

#[test]
fn same_size_restored_mtime_replacement_refreshes_catalog() {
    let w = World::in_process("replacement", cv_core::env::Env::enter_isolated);
    let path = write_session(&w, "-work-proj", "replaced", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions();
    let original = fs::metadata(&path).unwrap();
    let parent = path.parent().unwrap();
    let dir_time = fs::metadata(parent).unwrap().modified().unwrap();
    let text = fs::read_to_string(&path).unwrap();
    // Same encoded size, distinct demanded cwd, installed as a new inode.
    let replacement = text.replace("/work/proj", "/work/next");
    assert_eq!(replacement.len(), text.len());
    let temporary = path.with_extension("replacement");
    fs::write(&temporary, replacement).unwrap();
    fs::File::open(&temporary)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original.modified().unwrap()))
        .unwrap();
    fs::rename(&temporary, &path).unwrap();
    // Keep the directory watch unchanged: the primary identity must detect the replacement.
    fs::File::open(parent)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(dir_time))
        .unwrap();
    let listed = cv_core::sessions();
    let r = listed.iter().find(|r| r.id == "replaced").unwrap();
    assert_eq!(r.cwd.as_deref(), Some(std::path::Path::new("/work/next")));
}

#[test]
fn additive_row_without_captured_source_forces_refresh() {
    let w = World::in_process("uncaptured", cv_core::env::Env::enter_isolated);
    write_session(&w, "-work-proj", "alphasess", 2);
    cv_core::discover_all();
    let mut unknown = cv_core::catalog::lookup("alphasess", Some(Harness::Claude))
        .pop()
        .unwrap();
    unknown.id = "uncaptured-row".into();
    unknown.path = w.base.join("not-a-source.jsonl");
    cv_core::catalog::sync(&[unknown]);
    assert_eq!(ids(&cv_core::sessions()), vec!["alphasess"]);
}

#[cfg(feature = "sqlite")]
#[test]
fn full_discovery_avoids_unchanged_publication_but_enumerates_new_project_paths() {
    let w = World::in_process("discovery-publication", cv_core::env::Env::enter_isolated);
    let alpha = write_session(&w, "-project", "alpha", 2);
    let first = cv_core::discover_all();
    let db = rusqlite::Connection::open(w.cv_home.join("catalog.db")).unwrap();
    db.execute_batch("CREATE TABLE publication_audit(kind TEXT,table_name TEXT);")
        .unwrap();
    for table in ["sessions", "watched", "primary_sources", "scan_cache"] {
        for kind in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER audit_{table}_{kind} AFTER {kind} ON {table} BEGIN INSERT INTO publication_audit VALUES('{kind}','{table}'); END;")).unwrap();
        }
    }
    let writes = || {
        db.query_row("SELECT count(*) FROM publication_audit", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    let ordered = |refs: &[SessionRef]| serde_json::to_value(refs).unwrap();
    let second = cv_core::discover_all();
    assert_eq!(ordered(&first), ordered(&second));
    assert_eq!(writes(), 0, "unchanged full inventory must not mutate publication rows");

    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&alpha)
        .unwrap()
        .write_all(user_line("alpha", 2).as_bytes())
        .unwrap();
    let appended = cv_core::discover_all();
    assert_eq!(
        appended.iter().find(|r| r.id == "alpha").unwrap().message_count,
        Some(3)
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM publication_audit WHERE table_name='sessions'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM publication_audit WHERE table_name='scan_cache'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM publication_audit WHERE table_name='primary_sources'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    db.execute("DELETE FROM publication_audit", []).unwrap();
    assert_eq!(ordered(&appended), ordered(&cv_core::discover_all()));
    assert_eq!(writes(), 0);

    // A fresh inventory must find directories absent from the previous watch set.
    let beta = write_session(&w, "-new-deep-project", "beta", 1);
    let added = cv_core::discover_all();
    assert!(added.iter().any(|r| r.id == "beta"));
    assert!(writes() > 0);
    db.execute("DELETE FROM publication_audit", []).unwrap();
    fs::remove_file(beta).unwrap();
    let removed = cv_core::discover_all();
    assert!(!removed.iter().any(|r| r.id == "beta"));
    assert!(writes() > 0);
    let catalog_order: Vec<String> = db
        .prepare("SELECT id FROM sessions ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(catalog_order, removed.iter().map(|r| r.id.clone()).collect::<Vec<_>>());
}
