//! Acceptance against real data on the machine that built these commands — the orchestrating
//! session of 2026-09-30/10-01 (`0c315aee…`, ~164 sub-agents) and the live task store. These are
//! `#[ignore]`d: they run only when asked (`cargo test --test real_data -- --ignored`) and FAIL
//! when the data is not there, so they can never pass by skipping. The hermetic suites
//! (`orchestrate.rs`, `cli.rs`) are the gate; this file is the evidence the gate was aimed at
//! something real.

use std::path::PathBuf;
use std::process::Command;

const SESSION: &str = "0c315aee";

fn session_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    home.join(".claude/projects/-Users-ember-dev-breadstuffs/0c315aee-e07a-499c-8bb4-fa36730510ca.jsonl")
}

fn cv(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cv"))
        .args(args)
        .output()
        .expect("cv runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn require_real_session() {
    let p = session_path();
    assert!(p.exists(), "the real session is not on this machine: {}", p.display());
}

#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_prompts_are_the_38_human_lines_and_6_answers() {
    require_real_session();
    let (code, out) = cv(&["prompts", SESSION, "--json"]);
    assert_eq!(code, 0);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let prompts = rows.iter().filter(|r| r["kind"] == "prompt").count();
    let answers = rows.iter().filter(|r| r["kind"] == "answer").count();
    assert_eq!((prompts, answers), (38, 6), "{prompts} prompts, {answers} answers");
    assert!(rows[0]["text"].as_str().unwrap().starts_with("ok we have 13m to burn"));
    assert!(!rows
        .iter()
        .any(|r| r["text"].as_str().unwrap().starts_with("<command-name>")));
}

#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_lanes_census_matches_the_transcripts() {
    require_real_session();
    let (code, out) = cv(&["lanes", SESSION, "--json"]);
    assert_eq!(code, 0);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(rows.len() >= 164, "{} lanes", rows.len());
    let running = rows.iter().filter(|r| r["status"] == "running").count();
    let completed = rows.iter().filter(|r| r["status"] == "completed").count();
    assert!(completed >= 150, "{completed} completed");
    assert!(running <= 20, "{running} running");
    // Every lane parsed its model and spent tokens.
    assert!(rows
        .iter()
        .all(|r| r["model"].as_str().is_some_and(|m| m.starts_with("claude-"))));
    assert!(rows.iter().all(|r| r["tokens"]["total"].as_u64().unwrap() > 0));
}

#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_deferrals_cross_reference_the_live_task_store() {
    require_real_session();
    let (_, out) = cv(&["deferrals", SESSION, "--open-tasks", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(rows.len() >= 40, "{} deferrals", rows.len());
    let matched = rows.iter().filter(|r| r["matched"].is_object()).count();
    assert!(matched >= 20, "{matched} matched");
    assert!(rows.iter().any(|r| r["phrase"] == "after FINAL"));
}
