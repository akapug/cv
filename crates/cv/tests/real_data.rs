//! Acceptance against a real Claude Code session on whichever machine runs this. The hermetic
//! suites (`orchestrate.rs`, `cli.rs`) are the gate; this file is the evidence the gate was aimed
//! at something real. These are `#[ignore]`d and FAIL when not pointed at data, so they can never
//! pass by skipping:
//!
//! ```sh
//! CV_REAL_SESSION=<session id or prefix> cargo test --test real_data -- --ignored
//! ```
//!
//! The session should be an orchestrator with sub-agents (the commands under test are `show --kind prompt,answer`
//! and `lanes`); the assertions are the invariants any such session satisfies, not one
//! afternoon's counts.

use std::process::Command;

fn real_session() -> String {
    std::env::var("CV_REAL_SESSION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .expect("set CV_REAL_SESSION=<session id> to run the real-data tests")
}

fn cv(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_cv"))
        .args(args)
        .output()
        .expect("cv runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "cv {args:?}: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    stdout
}

#[test]
#[ignore = "needs CV_REAL_SESSION; run with --ignored"]
fn real_prompts_are_human_lines_and_answers_only() {
    let session = real_session();
    let out = cv(&["show", &session, "--kind", "prompt,answer", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(!rows.is_empty(), "a real session has at least one prompt");
    assert!(
        rows.iter()
            .all(|r| matches!(r["kind"].as_str().expect("message kind"), "prompt" | "answer")),
        "every row is a prompt or an AskUserQuestion answer"
    );
    assert!(rows.iter().any(|r| r["kind"] == "prompt"));
    // Slash-command bookkeeping is a `notice`, never a prompt (Claude Code >= 2.1 writes
    // `<command-name>…` records on the user side).
    assert!(!rows.iter().any(|r| r["text"]
        .as_str()
        .expect("prompt/answer text")
        .starts_with("<command-name>")));
}

#[test]
#[ignore = "needs CV_REAL_SESSION; run with --ignored"]
fn real_lanes_census_is_consistent() {
    let session = real_session();
    let out = cv(&["lanes", &session, "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(!rows.is_empty(), "the session has sub-agents (pick an orchestrator)");
    let running = rows.iter().filter(|r| r["status"] == "running").count();
    // The lost-notification class would make every finished lane read `running` after a harness
    // restart; a real forest is never all running.
    assert!(running < rows.len(), "{running} running of {}", rows.len());
    // Every lane parsed its model and spent tokens.
    assert!(rows
        .iter()
        .all(|r| r["model"].as_str().is_some_and(|m| m.starts_with("claude-"))));
    assert!(rows.iter().all(|r| r["tokens"]["total"].as_u64().unwrap() > 0));
}
