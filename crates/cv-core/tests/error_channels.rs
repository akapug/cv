//! Optional side stores distinguish absence from a failed read.
use cv_core::{Harness, SessionRef, SubagentDemand};
use std::fs;

#[test]
fn forest_errors_reach_readers_and_selected_metadata() {
    let world = cv_testkit::World::in_process("error-channels", cv_core::env::Env::enter_isolated);
    let path = world.home.join("parent.jsonl");
    let r = SessionRef::new("parent", Harness::Claude, &path);
    assert!(cv_core::subagent_tree_of(&r).unwrap().is_empty());
    let dir = path.with_extension("").join("subagents");
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    fs::write(&dir, "not a directory").unwrap();
    assert!(cv_core::subagent_tree_of(&r).is_err());
    fs::remove_file(&dir).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("agent-child.jsonl"),
        r#"{"type":"user","uuid":"u","sessionId":"agent-child","message":{"role":"user","content":"hello"}}
"#,
    )
    .unwrap();
    fs::write(dir.join("agent-child.meta.json"), "{broken").unwrap();
    assert_eq!(
        cv_core::subagent_tree_filtered_of(&r, SubagentDemand::Sessions, None, |_, _| true)
            .unwrap()
            .len(),
        1
    );
    assert!(cv_core::subagent_tree_of(&r).is_err());
    assert!(
        cv_core::subagent_tree_filtered_of(&r, SubagentDemand::Rich, None, |_, _| false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn subagent_refs_propagate_directory_and_database_failures() {
    let world = cv_testkit::World::in_process("error-channels", cv_core::env::Env::enter_isolated);
    let r = SessionRef::new("parent", Harness::Kimi, &world.home);
    fs::write(world.home.join("subagents"), "not a directory").unwrap();
    assert!(cv_core::harness::kimi::subagent_refs(&r).is_err());
    fs::write(world.home.join("agents"), "not a directory").unwrap();
    assert!(cv_core::harness::kimi_code::subagent_refs(&r).is_err());
    #[cfg(feature = "sqlite")]
    {
        let db = world.home.join("broken.db");
        fs::write(&db, "not a database").unwrap();
        let r = SessionRef::new("parent", Harness::Hermes, &db);
        assert!(cv_core::harness::hermes::subagent_refs(&r).is_err());
        assert!(cv_core::harness::devin::subagent_tree(&db, "parent").is_err());
    }
}

#[test]
fn json_files_and_store_affordances_report_corrupt_side_files() {
    let world = cv_testkit::World::in_process("affordance-errors", cv_core::env::Env::enter_isolated);
    let dir = world.home.join(".claude/tasks/parent");
    assert!(cv_core::affordance::json_files(&dir).unwrap().is_empty());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("1.json"), "{broken").unwrap();
    let error = cv_core::affordance::json_files(&dir).unwrap_err();
    assert!(format!("{error:#}").contains("1.json"));
    let r = SessionRef::new("parent", Harness::Claude, world.proj.join("parent.jsonl"));
    fs::write(&r.path, "{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"parent\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n").unwrap();
    let adapter = cv_core::harness::rootless(Harness::Claude).unwrap();
    assert!(adapter.store_affordances(&r).is_err());
    assert!(cv_core::affordance::read(adapter, &r).is_err());
    fs::write(dir.join("1.json"), r#"{"subject":"repair","status":"pending"}"#).unwrap();
    assert_eq!(adapter.store_affordances(&r).unwrap().len(), 1);
}

#[cfg(feature = "sqlite")]
#[test]
fn opencode_discovery_reports_a_row_decode_failure() {
    use cv_core::Adapter;
    let world = cv_testkit::World::in_process("opencode-row-error", cv_core::env::Env::enter_isolated);
    let db = world.home.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE session_v2(id, directory, title, time_created, time_updated); CREATE TABLE session_message(session_id, type); INSERT INTO session_v2 VALUES (x'ff', '', '', 0, 0);").unwrap();
    let adapter = cv_core::harness::opencode::OpenCode::with_db(db.clone());
    let discovered = adapter.discover().unwrap();
    assert!(discovered.refs.is_empty());
    assert_eq!(discovered.skipped.len(), 1);
    assert_eq!(discovered.skipped[0].path, db);
    assert!(discovered.skipped[0].why.contains("column"));
}

#[test]
fn claude_end_and_notices_do_not_turn_failed_reads_into_running_or_empty() {
    let world = cv_testkit::World::in_process("claude-status-errors", cv_core::env::Env::enter_isolated);
    let path = &world.home; // A directory is a deterministic read failure, including as root.
    assert!(cv_core::harness::claude::subagent_end(path).is_err());
    assert!(cv_core::harness::claude::task_notices(path).is_err());
    assert!(cv_core::harness::claude::subagent_return(path).is_err());
}

#[test]
fn string_stream_carries_a_named_consumer_failure() {
    struct Failed;
    impl cv_core::MessageSink for Failed {
        fn message(&mut self, _: cv_core::Message) -> cv_core::Flow {
            cv_core::Flow::Stop
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            Some(anyhow::anyhow!("injected consumer failure"))
        }
    }
    let session = cv_core::harness::claude::stream_str("fixture", "{}\n", None, &cv_core::Demand::RICH, &mut Failed);
    assert!(session.extra["cv"]["read_error"]
        .as_str()
        .unwrap()
        .contains("injected consumer failure"));
}

#[test]
fn workflow_launches_report_an_aborted_scan() {
    let world = cv_testkit::World::in_process("workflow-launch-error", cv_core::env::Env::enter_isolated);
    assert!(cv_core::harness::claude_workflow::launches(&world.home).is_err());
}

#[test]
fn codex_string_input_counts_an_invalid_legacy_document() {
    let session = cv_core::harness::codex::parse_str("fixture", "{broken", false, None);
    assert_eq!(session.extra["cv"]["skipped_lines"], 1);
}
