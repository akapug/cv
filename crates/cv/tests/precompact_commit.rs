//! The commit a harness plugin calls inside its own compaction, end to end through the binary: a
//! ready record in a world's store, the `session.compact` input on stdin, the summary and where
//! the kept messages start on stdout, or exit 3 with the miss class; then `observe` and `report`.

use cv_core::precompact::{anchor, store, Cut, Key, Record, WindowRef, RECORD_VERSION};
use cv_testkit::World;
use serde_json::{json, Value};

const SESSION: &str = "s-1";

/// A ready record for the session, cut after the assistant's tool call `toolu_X`.
fn ready(w: &World) {
    let transcript = w.proj.join(format!("{SESSION}.jsonl"));
    std::fs::write(&transcript, "{\"uuid\":\"u1\"}\n{\"uuid\":\"a1\"}\n").unwrap();
    let record = Record {
        v: RECORD_VERSION,
        harness: "claude".into(),
        session: SESSION.into(),
        key: Key::take(&transcript, "a1".into()),
        window: WindowRef {
            boundaries: 0,
            start: 0,
        },
        cut: Cut {
            index: 1,
            id: Some("a1".into()),
            tokens: Some(1000),
            clean: true,
        },
        anchors: vec![
            anchor::Anchor::of("please refactor the parser module"),
            anchor::Anchor::of("I will refactor the parser module now."),
            anchor::Anchor::of(&anchor::call_marker("toolu_X").unwrap()),
        ],
        profile: String::new(),
        model: "m".into(),
        created: chrono::Utc::now(),
        base: None,
        compact_at: Some(200_000),
        bounds: Vec::new(),
        narrative: "N".into(),
        carry: Default::default(),
        text: "the cv summary [m:a1]".into(),
    };
    let path = store::record_path(&w.cv_home.join("precompact"), "claude", SESSION);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
}

fn event(reply: &str) -> String {
    json!({
        "session": SESSION, "harness": "claude", "trigger": "auto",
        "messages": [
            {"role": "user", "text": "please refactor the parser module", "toolUses": []},
            {"role": "assistant", "text": "I will refactor the parser module now.",
             "toolUses": [{"tool_use_id": "toolu_X", "tool": "Bash"}]},
            {"role": "user", "text": "", "toolUses": [], "toolResults": [{"tool_use_id": "toolu_X"}]},
            {"role": "assistant", "text": reply, "toolUses": []}
        ]
    })
    .to_string()
}

const COMMIT: [&str; 6] = ["watch", "--kind", "precompact", "commit", "--serve", "cv"];

#[test]
fn a_ready_record_is_committed_inside_the_harness_compaction_and_every_miss_exits_3() {
    let w = World::new("precompact-commit");
    let (ok, code, _, err) = w.cv_stdin(&COMMIT, &event("Done."));
    assert_eq!((ok, code), (false, 3), "no record yet is a miss: {err}");
    assert!(err.contains("(no_record)"), "{err}");

    ready(&w);
    let (ok, code, out, err) = w.cv_stdin(&COMMIT, &event("Done."));
    assert!(ok, "exit {code}: {err}");
    let served: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(served["outcome"], "served");
    // The cut is the call; its result follows the cut, so the kept messages start at the call.
    assert_eq!(served["keep_from"], 1);
    let summary = served["summary"].as_str().unwrap();
    assert!(summary.ends_with("the cv summary [m:a1]"), "{summary}");
    assert!(summary.contains("cv show s-1 --around ID"), "{summary}");

    // The seat serves the harness's own: a miss, whatever cv holds.
    let mut native = COMMIT.to_vec();
    native[5] = "native";
    let (_, code, _, err) = w.cv_stdin(&native, &event("Done."));
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("(serve_native)"), "{err}");

    // A covered message changed in the conversation the harness holds.
    let changed = event("Done.").replace("I will refactor the parser module now.", "Something else entirely.");
    let (_, code, _, err) = w.cv_stdin(&COMMIT, &changed);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("(covered_messages_changed)"), "{err}");

    // The plugin records each compaction; `report` compares the methods.
    let row = json!({"session": SESSION, "trigger": "auto", "method": "cv", "ms": 40,
                     "tokens_before": 180_000, "tokens_after": 21_000, "summary_bytes": 900});
    w.cv_stdin(&["watch", "--kind", "precompact", "observe"], &row.to_string());
    let (out, _) = w.cv_ok(&["watch", "--kind", "precompact", "report", "--json"]);
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["commits_served"], 1);
    assert_eq!(report["commits_missed"], 3);
    assert_eq!(report["miss_classes"]["serve_native"], 1);
    let cv_row = report["methods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["source"] == "plugin" && r["method"] == "cv")
        .unwrap();
    assert_eq!(
        (cv_row["compactions"].as_u64(), cv_row["median_tokens_after"].as_u64()),
        (Some(1), Some(21_000))
    );

    // The proxy-era action is gone, with a pointer.
    let (_, code, _, err) = w.cv(&["watch", "--kind", "precompact", "answer"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("commit"), "{err}");
}

/// A record written before a compaction no follower saw (a restarted follower, or `now` run
/// once per window) still hands its carried sections to the next window: the next `now` folds
/// them into the session's carry before it summarizes. A dry run folds nothing.
#[test]
fn a_record_from_an_earlier_window_becomes_the_next_windows_carry() {
    use cv_core::precompact::carry::{Carry, Item, Section};
    let w = World::new("precompact-fold");
    let sid = "aaaaaaaa-0000-4000-8000-0000000000f1";
    let line = |ty: &str, uuid: &str, parent: Option<&str>, message: Value| {
        json!({"type": ty, "sessionId": sid, "uuid": uuid, "parentUuid": parent, "isSidechain": false,
            "cwd": "/work/proj", "timestamp": "2026-10-29T10:00:00Z", "message": message})
    };
    let reply = |text: &str| {
        json!({"role": "assistant", "model": "m", "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 10, "output_tokens": 5}})
    };
    w.write_session(
        sid,
        &[
            line(
                "user",
                "u1",
                None,
                json!({"role": "user", "content": "never push main"}),
            ),
            line("assistant", "a1", Some("u1"), reply("Understood.")),
            json!({"type": "system", "subtype": "compact_boundary", "sessionId": sid, "uuid": "b1",
                "parentUuid": "a1", "isSidechain": false, "compactMetadata": {"trigger": "auto", "preTokens": 100}}),
            line(
                "user",
                "u2",
                Some("b1"),
                json!({"role": "user", "content": "now ship the port"}),
            ),
            line("assistant", "a2", Some("u2"), reply("Shipping it.")),
        ],
    );
    let ruling = Item {
        section: Section::Rule,
        quote: "never push main".into(),
        m: "u1".into(),
        why: String::new(),
    };
    let record = Record {
        v: RECORD_VERSION,
        harness: "claude".into(),
        session: sid.into(),
        key: None,
        window: WindowRef {
            boundaries: 0,
            start: 0,
        },
        cut: Cut {
            index: 1,
            id: Some("a1".into()),
            tokens: Some(15),
            clean: true,
        },
        anchors: Vec::new(),
        profile: String::new(),
        model: "m".into(),
        created: chrono::Utc::now(),
        base: None,
        compact_at: None,
        bounds: Vec::new(),
        narrative: "N".into(),
        carry: Carry {
            items: vec![ruling.clone()],
            ..Default::default()
        },
        text: "N".into(),
    };
    let root = w.cv_home.join("precompact");
    let path = store::record_path(&root, "claude", sid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    // No model answers here: the summary pass fails, after the fold.
    let none = [("LMSTUDIO_API_BASE", "http://127.0.0.1:9/v1")];
    w.cv_env(&["watch", "--kind", "precompact", "now", sid, "--dry-run"], &none);
    assert!(
        store::load_record(&root, "claude", sid).unwrap().is_some(),
        "a dry run folds nothing"
    );
    assert!(store::load_carry(&root, "claude", sid).is_none());
    w.cv_env(&["watch", "--kind", "precompact", "now", sid], &none);
    assert_eq!(store::load_carry(&root, "claude", sid).unwrap().items, vec![ruling]);
    assert!(store::load_record(&root, "claude", sid).unwrap().is_none());
}
