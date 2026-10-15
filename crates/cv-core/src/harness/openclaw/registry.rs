//! The state dir's registries beside the transcripts: the cron registry and the sub-agent
//! registry, read as a session's `schedule`/`state` and `subagent_spawn`/`state` rows
//! ([`crate::harness::Adapter::store_affordances`]).
//!
//! Both are keyed by **session key** (`agent:<agentId>:<kind>:<…>`), never by session id, so the
//! session's key is looked up first (`sessions.json` beside a JSONL transcript, `session_windows`
//! in the sqlite store). Layout by era, under the state dir (`agents/`'s parent):
//!
//! - **JSONL era (2026.1.29):** `cron/jobs.json` `{version, jobs: [{id, agentId, name, enabled,
//!   createdAtMs, schedule: {kind: at|cron|every, atMs|at|expr|everyMs}, sessionTarget, payload:
//!   {kind, message}, state: {lastRunAtMs, lastStatus}}]}` (+ `cron/runs/<jobId>.jsonl`, the run
//!   log), and `subagents/runs.json` `{version, runs: {<runId>: {childSessionKey,
//!   requesterSessionKey, task, label?, createdAt, endedAt?, outcome: {status}}}}`.
//! - **sqlite era (2026.10.1):** `state/openclaw.sqlite` `cron_jobs(job_json, state_json, …)`
//!   (`job_json` is the old `jobs.json` entry, plus `owner: {sessionKey}` for a job an agent
//!   made) and `subagent_runs(requester_session_key, controller_session_key, payload_json)`
//!   (`payload_json`: `task`, `label`, `execution.outcome.status`, `completion.resultText`).
//!
//! A job belongs to a session when the session made it (`owner.sessionKey`,
//! `scheduledToolPolicy.ownerSessionKey`), when the session is one of its runs
//! (`agent:<a>:cron:<jobId>[:run:<id>]`), or when it fires into the session (`sessionTarget:
//! "main"` and the key `agent:<agentId>:main`). A 1.29 job records no maker, so its maker's `cron`
//! tool call is its only trace there. A sub-agent run belongs to the session that requested or
//! controls it; the child already names its parent through `Lineage::parent`.

use crate::affordance::{Affordance, Kind};
use crate::ir::{truncate, SessionRef};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Every registry row that belongs to `r`.
pub(super) fn affordances(r: &SessionRef) -> Vec<Affordance> {
    // `<state>/agents/<agent>/sessions/<sid>.jsonl` and `<state>/agents/<agent>/agent/<db>.sqlite`
    // sit at the same depth.
    let Some(state) = r.path.ancestors().nth(4) else {
        return Vec::new();
    };
    let Some(key) = session_key(r) else {
        return Vec::new();
    };
    let (jobs, runs) = if super::is_sqlite_store_path(&r.path) {
        sqlite_registries(state, &key)
    } else {
        (jsonl_jobs(state), jsonl_runs(state, &key))
    };
    let mut out: Vec<Affordance> = jobs.iter().filter(|j| job_belongs(j, &key)).map(job_row).collect();
    out.extend(runs.iter().filter(|(by, _)| by.contains(&key)).map(|(_, p)| run_row(p)));
    out
}

/// The session's key: the `sessions.json` entry naming it, or its `session_windows` row.
fn session_key(r: &SessionRef) -> Option<String> {
    if super::is_sqlite_store_path(&r.path) {
        return sqlite_session_key(r);
    }
    let text = fs::read_to_string(r.path.parent()?.join("sessions.json")).ok()?;
    let Ok(Value::Object(index)) = serde_json::from_str::<Value>(&text) else {
        return None;
    };
    index
        .iter()
        .find(|(_, e)| e.get("sessionId").and_then(Value::as_str) == Some(r.id.as_str()))
        .map(|(k, _)| k.clone())
}

#[cfg(feature = "sqlite")]
fn sqlite_session_key(r: &SessionRef) -> Option<String> {
    let conn = super::open_ro(&r.path).ok()?;
    conn.read(|conn| {
        Ok(conn.query_row(
            "SELECT session_key FROM session_windows WHERE session_id = ?1",
            [&r.id],
            |row| row.get(0),
        )?)
    })
    .ok()
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_session_key(_r: &SessionRef) -> Option<String> {
    None
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn jsonl_jobs(state: &Path) -> Vec<Value> {
    let Some(mut registry) = read_json(&state.join("cron").join("jobs.json")) else {
        return Vec::new();
    };
    match registry.get_mut("jobs").map(Value::take) {
        Some(Value::Array(jobs)) => jobs,
        _ => Vec::new(),
    }
}

/// A sub-agent run: the keys it belongs to (requester, controller) and its record.
type Run = (Vec<String>, Value);

fn run_keys(p: &Value) -> Vec<String> {
    ["requesterSessionKey", "controllerSessionKey"]
        .iter()
        .filter_map(|k| p.get(*k).and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn jsonl_runs(state: &Path, key: &str) -> Vec<Run> {
    let Some(mut registry) = read_json(&state.join("subagents").join("runs.json")) else {
        return Vec::new();
    };
    let Some(Value::Object(runs)) = registry.get_mut("runs").map(Value::take) else {
        return Vec::new();
    };
    // Only this session's runs need ordering. Filtering before sorting avoids ordering
    // the full global registry again for every requested session.
    let mut out: Vec<Run> = runs
        .into_iter()
        .filter_map(|(_, p)| {
            ["requesterSessionKey", "controllerSessionKey"]
                .into_iter()
                .any(|field| p.get(field).and_then(Value::as_str) == Some(key))
                .then(|| (run_keys(&p), p))
        })
        .collect();
    out.sort_by_key(|(_, p)| p.get("createdAt").and_then(Value::as_i64).unwrap_or(0));
    out
}

#[cfg(feature = "sqlite")]
fn sqlite_registries(state: &Path, key: &str) -> (Vec<Value>, Vec<Run>) {
    use super::super::store::sqlite::table_exists;
    let Ok(conn) = super::open_ro(&state.join("state").join("openclaw.sqlite")) else {
        return Default::default();
    };
    conn.read(|conn| {
        let parse = |t: String| serde_json::from_str::<Value>(&t).ok();
        let jobs = if table_exists(conn, "cron_jobs") {
            // Decode and filter each row before retaining it. Payload ownership remains authoritative:
            // migrated projections can disagree, and SQLite's JSON duplicate-key rules differ from
            // serde_json's. Unrelated bodies live only until the next cursor row.
            conn.prepare("SELECT job_json, state_json FROM cron_jobs ORDER BY sort_order, updated_at, job_id")
                .and_then(|mut st| {
                    st.query_map([], |row| {
                        let job: String = row.get(0)?;
                        let state: String = row.get(1)?;
                        let Some(mut job) = parse(job).filter(|job| job_belongs(job, key)) else {
                            return Ok(None);
                        };
                        // The runtime state rides in its own column; fold it back where the 1.29
                        // entry kept it, but only decode state for matching jobs.
                        if let (Some(o), Some(state)) = (job.as_object_mut(), parse(state)) {
                            o.insert("state".into(), state);
                        }
                        Ok(Some(job))
                    })?
                    .filter_map(|row| row.transpose())
                    .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let runs = if table_exists(conn, "subagent_runs") {
            conn.prepare("SELECT payload_json FROM subagent_runs ORDER BY created_at, run_id")
                .and_then(|mut st| {
                    st.query_map([], |row| {
                        let payload: String = row.get(0)?;
                        let Some(payload) = parse(payload) else {
                            return Ok(None);
                        };
                        let keys = run_keys(&payload);
                        Ok(keys.iter().any(|by| by == key).then_some((keys, payload)))
                    })?
                    .filter_map(|row| row.transpose())
                    .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok((jobs, runs))
    })
    .unwrap_or_default()
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_registries(_state: &Path, _key: &str) -> (Vec<Value>, Vec<Run>) {
    Default::default()
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

fn job_belongs(job: &Value, key: &str) -> bool {
    let owner = ["/owner/sessionKey", "/scheduledToolPolicy/ownerSessionKey"]
        .iter()
        .any(|p| str_at(job, p) == Some(key));
    let id = str_at(job, "/id").unwrap_or("");
    let mut parts = key.split(':');
    let (agent, kind, rest) = (parts.nth(1), parts.next(), parts.next());
    let run = kind == Some("cron") && !id.is_empty() && rest == Some(id);
    let main = str_at(job, "/sessionTarget") == Some("main")
        && kind == Some("main")
        && agent.is_some()
        && str_at(job, "/agentId") == agent;
    owner || run || main
}

/// The public summary is 120 characters; retain one lookahead for truncate's ellipsis.
fn summary_prefix<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts.into_iter().flat_map(str::chars).take(121).collect()
}

/// `corpus-ping · at 2026-10-11T20:48:42Z · Reply with the word pong. · last ok`.
fn job_row(job: &Value) -> Affordance {
    let when = match str_at(job, "/schedule/kind") {
        Some("cron") => str_at(job, "/schedule/expr").map(|s| summary_prefix([s])),
        Some("every") => job
            .pointer("/schedule/everyMs")
            .and_then(Value::as_i64)
            .map(|ms| format!("every {}s", ms / 1000)),
        Some("at") => str_at(job, "/schedule/at")
            .map(|a| summary_prefix(["at ", a]))
            .or_else(|| {
                job.pointer("/schedule/atMs")
                    .and_then(Value::as_i64)
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|t| format!("at {}", t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
            }),
        _ => None,
    };
    let name = str_at(job, "/displayName").or_else(|| str_at(job, "/name"));
    let message = str_at(job, "/payload/message").or_else(|| str_at(job, "/payload/kind"));
    let last = str_at(job, "/state/lastStatus").map(|s| summary_prefix(["last ", s]));
    let disabled = (job.get("enabled").and_then(Value::as_bool) == Some(false)).then_some("disabled");
    let parts: Vec<&str> = [name, when.as_deref(), message, last.as_deref(), disabled]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    let prefix = summary_prefix(
        parts
            .iter()
            .enumerate()
            .flat_map(|(i, part)| [if i == 0 { "" } else { " · " }, *part]),
    );
    Affordance::store(Kind::Schedule, "cron", truncate(&prefix, 120), Vec::new())
}

/// `ok-reply-check · Reply with the single word ok. · ok → ok`.
fn run_row(p: &Value) -> Affordance {
    let who = str_at(p, "/label")
        .or_else(|| str_at(p, "/childSessionKey"))
        .unwrap_or("");
    let task = str_at(p, "/task").unwrap_or("");
    let status = str_at(p, "/execution/outcome/status").or_else(|| str_at(p, "/outcome/status"));
    let mut parts = vec![who, " · ", task];
    if let Some(s) = status {
        parts.extend([" · ", s]);
    }
    if let Some(res) = str_at(p, "/completion/resultText") {
        parts.extend([" → ", res]);
    }
    let summary = summary_prefix(parts);
    let timestamp = p
        .get("createdAt")
        .and_then(Value::as_i64)
        .and_then(chrono::DateTime::from_timestamp_millis);
    Affordance {
        timestamp,
        ..Affordance::store(Kind::SubagentSpawn, "subagents", truncate(&summary, 120), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The three ways a job belongs to a session: it made the job, it is one of the job's runs, or
    /// the job fires into it.
    #[test]
    fn a_job_belongs_to_its_owner_its_runs_and_its_main_target() {
        let made = json!({"id": "j1", "owner": {"sessionKey": "agent:main:explicit:s1"}, "sessionTarget": "isolated"});
        assert!(job_belongs(&made, "agent:main:explicit:s1"));
        assert!(job_belongs(&made, "agent:main:cron:j1"));
        assert!(job_belongs(&made, "agent:main:cron:j1:run:r9"));
        assert!(!job_belongs(&made, "agent:main:cron:j2"));
        assert!(!job_belongs(&made, "agent:main:main"));
        let heartbeat = json!({"id": "hb", "agentId": "main", "sessionTarget": "main"});
        assert!(job_belongs(&heartbeat, "agent:main:main"));
        assert!(!job_belongs(&heartbeat, "agent:other:main"));
        let every = json!({"name": "hb", "schedule": {"kind": "every", "everyMs": 1_800_000}, "payload": {"kind": "heartbeat"}});
        assert_eq!(job_row(&every).summary, "hb · every 1800s · heartbeat");
    }

    #[test]
    fn jsonl_run_order_and_ownership_match_global_sort_with_many_unrelated_runs() -> anyhow::Result<()> {
        let world = crate::test_world("openclaw-owned-run-order");
        fs::create_dir_all(world.base.join("subagents"))?;
        let key = "agent:main:explicit:requested";
        let mut runs = serde_json::Map::new();
        for i in 0..5000 {
            runs.insert(
                format!("other-{i}"),
                json!({"requesterSessionKey":"unrelated",
            "createdAt":5000-i,"task":"unrelated"}),
            );
        }
        for (id, payload) in [
            ("late", json!({"requesterSessionKey":key,"createdAt":9})),
            ("early", json!({"controllerSessionKey":key,"createdAt":-9})),
            ("tie-a", json!({"requesterSessionKey":key,"createdAt":0})),
            (
                "tie-b",
                json!({"requesterSessionKey":"other","controllerSessionKey":key,"createdAt":0}),
            ),
            (
                "invalid",
                json!({"requesterSessionKey":false,"controllerSessionKey":1,"createdAt":-100}),
            ),
            ("missing-time", json!({"requesterSessionKey":key,"createdAt":"invalid"})),
        ] {
            runs.insert(id.into(), payload);
        }
        let mut expected: Vec<Run> = runs.values().cloned().map(|p| (run_keys(&p), p)).collect();
        expected.sort_by_key(|(_, p)| p.get("createdAt").and_then(Value::as_i64).unwrap_or(0));
        expected.retain(|(keys, _)| keys.iter().any(|by| by == key));
        fs::write(world.base.join("subagents/runs.json"), json!({"runs":runs}).to_string())?;
        let actual = jsonl_runs(&world.base, key);
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 5);
        Ok(())
    }

    #[test]
    fn registry_summaries_match_full_rendering_for_large_unicode_fields() {
        let giant = "雪λ\n".repeat(50000);
        for name in ["", "short", giant.as_str()] {
            for payload in ["", "short", giant.as_str()] {
                let job = json!({"name":name,"schedule":{"kind":"at","at":giant},
                    "payload":{"message":payload},"state":{"lastStatus":giant},"enabled":false});
                let expected = [
                    name.to_owned(),
                    format!("at {giant}"),
                    payload.to_owned(),
                    format!("last {giant}"),
                    "disabled".into(),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
                assert_eq!(job_row(&job).summary, truncate(&expected, 120));
                let run = json!({"label":name,"task":payload,"outcome":{"status":giant},
                    "completion":{"resultText":giant},"createdAt":1704067200000_i64});
                assert_eq!(
                    run_row(&run).summary,
                    truncate(&format!("{name} · {payload} · {giant} → {giant}"), 120)
                );
                assert_eq!(
                    run_row(&run).timestamp,
                    chrono::DateTime::from_timestamp_millis(1704067200000)
                );
            }
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_filters_match_payload_ownership_and_keep_row_order() {
        let state = std::env::temp_dir().join(format!("cv-openclaw-registry-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(state.join("state")).unwrap();
        let conn = rusqlite::Connection::open(state.join("state/openclaw.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE cron_jobs(job_json TEXT, state_json TEXT, sort_order INT, updated_at INT, job_id TEXT);\
             CREATE TABLE subagent_runs(payload_json TEXT, created_at INT, run_id TEXT, requester_session_key TEXT, controller_session_key TEXT);",
        ).unwrap();
        let mut jobs = vec![
            json!({"id":"j1", "owner":{"sessionKey":"agent:main:explicit:s1"}}),
            json!({"id":"hb", "agentId":"main", "sessionTarget":"main"}),
            json!({"id":"j2", "scheduledToolPolicy":{"ownerSessionKey":"agent:main:explicit:s1"}}),
            json!({"id":"", "owner":{"sessionKey":""}}),
            json!({"owner":{"sessionKey":1}, "agentId":1, "sessionTarget":"main"}),
            json!({"id":"orphan", "owner":{"sessionKey":"another-session"}}),
        ];
        let mut runs = vec![
            json!({"requesterSessionKey":"agent:main:explicit:s1", "task":"requested"}),
            json!({"controllerSessionKey":"agent:main:explicit:s1", "task":"controlled"}),
            json!({"requesterSessionKey":1, "controllerSessionKey":false}),
            json!({"requesterSessionKey":"another-session", "task":"unrelated"}),
        ];
        let raw_jobs = [
            r#"{"id":"duplicate-owner","owner":{"sessionKey":"other"},"owner":{"sessionKey":"agent:main:explicit:s1"}}"#,
            r#"{"id":"duplicate-session","owner":{"sessionKey":"other","sessionKey":"agent:main:explicit:s1"}}"#,
            r#"{"id":"duplicate-agent","sessionTarget":"main","agentId":"other","agentId":"main"}"#,
            r#"{"id":"escaped-owner","\u006fwner":{"session\u004bey":"agent:main:explicit:s1"}}"#,
            r#"{"id":"nested-unrelated","payload":{"owner":{"sessionKey":"agent:main:explicit:s1"}}}"#,
        ];
        let raw_runs = [
            r#"{"requesterSessionKey":"other","requesterSessionKey":"agent:main:explicit:s1"}"#,
            r#"{"controllerSessionKey":"other","controllerSessionKey":"agent:main:explicit:s1"}"#,
            r#"{"requesterSession\u004bey":"agent:main:explicit:s1"}"#,
            r#"{"payload":{"requesterSessionKey":"agent:main:explicit:s1"}}"#,
        ];
        let job_text: Vec<String> = jobs
            .iter()
            .map(Value::to_string)
            .chain(raw_jobs.iter().map(|s| s.to_string()))
            .collect();
        let run_text: Vec<String> = runs
            .iter()
            .map(Value::to_string)
            .chain(raw_runs.iter().map(|s| s.to_string()))
            .collect();
        jobs.extend(raw_jobs.iter().map(|s| serde_json::from_str::<Value>(s).unwrap()));
        runs.extend(raw_runs.iter().map(|s| serde_json::from_str::<Value>(s).unwrap()));
        for (i, job) in job_text.iter().enumerate() {
            conn.execute(
                "INSERT INTO cron_jobs VALUES (?1, ?2, ?3, 0, ?4)",
                rusqlite::params![job, r#"{"lastStatus":"ok"}"#, i, format!("job{i}")],
            )
            .unwrap();
        }
        for (i, run) in run_text.iter().enumerate() {
            // A relational projection disagrees with JSON. Preserve the original JSON-based
            // ownership contract instead of losing relevant runs or accepting unrelated ones.
            conn.execute(
                "INSERT INTO subagent_runs VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![run, i, format!("run{i}"), "wrong-projection"],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO cron_jobs VALUES ('malformed', '{}', 99, 0, 'bad')", [])
            .unwrap();
        conn.execute("INSERT INTO subagent_runs VALUES ('malformed', 99, 'bad', '', '')", [])
            .unwrap();
        for key in [
            "agent:main:explicit:s1",
            "agent:main:main",
            "agent:other:main",
            "agent:main:cron:j1:run:r9",
            "1",
            "",
        ] {
            let (actual_jobs, actual_runs) = sqlite_registries(&state, key);
            let expected_jobs: Vec<Value> = jobs
                .iter()
                .filter(|j| job_belongs(j, key))
                .cloned()
                .map(|mut j| {
                    j.as_object_mut()
                        .unwrap()
                        .insert("state".into(), json!({"lastStatus":"ok"}));
                    j
                })
                .collect();
            let expected_runs: Vec<Run> = runs
                .iter()
                .filter(|r| run_keys(r).iter().any(|k| k == key))
                .cloned()
                .map(|p| (run_keys(&p), p))
                .collect();
            assert_eq!(actual_jobs, expected_jobs, "{key}");
            assert_eq!(actual_runs, expected_runs, "{key}");
        }
        fs::remove_dir_all(state).unwrap();
    }
}
