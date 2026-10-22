//! Roo Code adapter — a Cline fork with the **same** per-task file format.
//!
//! Roo stores tasks under a different VS Code extension namespace
//! (`rooveterinaryinc.roo-cline` and the newer `rooveterinaryinc.roo-code`) plus a plain `~/.roo/`,
//! and the Roo CLI writes the same layout under `~/.vscode-mock/global-storage/tasks/`.
//! The on-disk layout inside each `tasks/<taskId>/` is identical to Cline's
//! (`api_conversation_history.json` array of Anthropic messages + `ui_messages.json` +
//! `task_metadata.json`), so all real *parse* logic lives in [`super::cline`] as `pub` helpers and we
//! just point them at Roo's roots and tag the output `Harness::Roo`.
//!
//! # Emit (conversion target)
//!
//! [`emit`] is [`super::cline::emit_with_harness`] with `Harness::Roo`: the per-task layout is the
//! same bytes for both harnesses, so Roo owns no writer of its own — only the resume hint names the
//! editor extension to open the task in.

use super::cline::{discover_in, emit_with_harness, stream_task_dir, task_dir_dependencies, task_roots, ROO_EXT_IDS};
use super::Adapter;
use crate::ir::*;
use crate::source::{Dependencies, Probe};
use crate::stream::{Demand, MessageSink};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Roo {
    roots: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Roo = Roo { roots: Vec::new() };

impl Roo {
    pub fn detect() -> Self {
        let mut roots = task_roots(ROO_EXT_IDS, ".roo");
        roots.extend(crate::env::home_dir().and_then(|h| cli_tasks_root(&h)));
        Roo { roots }
    }

    /// Roo's task stores under an explicit home: the Roo CLI's mock-VS-Code globalStorage and a
    /// plain `.roo/tasks` (the editor globalStorage dirs are platform paths, not home-relative).
    pub fn with_home(home: &Path) -> Self {
        let mut roots: Vec<PathBuf> = cli_tasks_root(home).into_iter().collect();
        let plain = home.join(".roo").join("tasks");
        if plain.is_dir() {
            roots.push(plain);
        }
        Roo { roots }
    }
}

/// The Roo Code CLI (GitHub `cli-v*` releases) runs the extension in a mock VS Code host whose
/// globalStorage is `~/.vscode-mock/global-storage`, so its tasks land in `tasks/` there with the
/// extension's own layout.
fn cli_tasks_root(home: &Path) -> Option<PathBuf> {
    let p = home.join(".vscode-mock").join("global-storage").join("tasks");
    p.is_dir().then_some(p)
}

impl Adapter for Roo {
    fn harness(&self) -> Harness {
        Harness::Roo
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        task_dir_dependencies(r, deps);
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        Ok(discover_in(&self.roots, Harness::Roo))
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Roo, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            stream_task_dir(&r.path, &r.id, Harness::Roo, opts, sink)
        })
    }

    /// The checkpoints and permission asks `ui_messages.json` logs (the conversation file has
    /// neither): `say: checkpoint_saved` (a shadow-git commit under `checkpoints/`) and the `ask`s
    /// that gate a tool, a command, an MCP call or a browser launch.
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        ui_affordances(&r.path.join("ui_messages.json"))
    }
}

fn ui_affordances(path: &Path) -> Result<Vec<crate::affordance::Affordance>> {
    use crate::affordance::{Affordance, Kind};
    use serde_json::Value;
    let Some(Value::Array(events)) = crate::affordance::side_json(path)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in &events {
        if e.get("partial").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let ts = e
            .get("ts")
            .and_then(Value::as_i64)
            .and_then(chrono::DateTime::from_timestamp_millis);
        let text = e.get("text").and_then(Value::as_str).unwrap_or("");
        // An ask's `text` is JSON for a tool or MCP call, the bare command line for a command.
        let json: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let field = |k: &str| json.get(k).and_then(Value::as_str).unwrap_or("");
        match (
            e.get("type").and_then(Value::as_str),
            e.get("ask").or(e.get("say")).and_then(Value::as_str),
        ) {
            (Some("say"), Some("checkpoint_saved")) => {
                let commit = text.get(..12).unwrap_or(text);
                out.push(Affordance::store_event(
                    Kind::Checkpoint,
                    "create",
                    ts,
                    "checkpoint_saved",
                    commit,
                ));
            }
            (Some("ask"), Some(what @ ("tool" | "command" | "use_mcp_server" | "browser_action_launch"))) => {
                let summary = match what {
                    "tool" => format!("{} {}", field("tool"), field("path")),
                    "use_mcp_server" => format!("{}.{}", field("serverName"), field("toolName")),
                    _ => text.to_string(),
                };
                out.push(Affordance::store_event(Kind::Approval, "ask", ts, what, summary.trim()));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Emit `session` into Roo's per-task layout under `out_dir`: the Cline writer, tagged for Roo.
/// Registered in `emit::emitter_for`.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult> {
    emit_with_harness(session, out_dir, opts, Harness::Roo)
}

#[cfg(test)]
mod tests {
    use super::super::cline::{parse_history_str, parse_task_dir};
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/roo/");
        std::fs::read_to_string(format!("{path}{name}")).unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    #[test]
    fn roo_reuses_cline_parser_and_tags_harness() {
        let s = parse_history_str(
            "1717111111111",
            &fixture("api_conversation_history.json"),
            Harness::Roo,
            None,
        );
        assert_eq!(s.harness, Harness::Roo);
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(s.title.as_deref(), Some("Add a unit test"));
        // user, assistant, tool turn.
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[2].role, Role::Tool);
    }

    /// Build a small Session, emit it to a unique temp dir, re-parse with this adapter, and assert
    /// message count + roles + tool blocks survive the round-trip.
    #[test]
    fn emit_round_trips_through_parser() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        // ── build a small session ──
        let mut user = Message::new(Role::User);
        user.content = vec![Block::Text {
            text: "Please add a test.".into(),
        }];

        let mut asst = Message::new(Role::Assistant);
        asst.content = vec![
            Block::Thinking {
                text: "I should write a test.".into(),
                signature: Some("SIG==".into()),
                encrypted: None,
                redacted: false,
            },
            Block::Text { text: "On it.".into() },
            Block::ToolUse {
                id: "toolu_42".into(),
                name: "write_to_file".into(),
                input: serde_json::json!({ "path": "src/lib.rs" }).into(),
                namespace: None,
            },
        ];

        let mut tool = Message::new(Role::Tool);
        tool.content = vec![Block::ToolResult {
            tool_use_id: "toolu_42".into(),
            content: "File written.".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        }];

        let session = Session {
            cwd: Some(PathBuf::from("/home/dev/app")),
            title: Some("Add a unit test".into()),
            messages: vec![user, asst, tool],
            ..Session::new("src", Harness::Roo)
        };

        // ── emit to a unique temp dir (no tempfile crate) ──
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let out_dir = std::env::temp_dir().join(format!("cv-roo-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&out_dir);

        let result = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).expect("emit roo session");

        // emit returns the task DIR (tasks/<taskId>/), holding api_conversation_history.json.
        let task_dir = result.path.as_path();
        assert_eq!(
            task_dir.file_name().and_then(|s| s.to_str()),
            Some(result.new_id.as_str())
        );
        assert!(task_dir.join("api_conversation_history.json").exists());

        // ── re-parse with this adapter and assert structure survives ──
        let reparsed = parse_task_dir(task_dir, &result.new_id, Harness::Roo).expect("re-parse emitted roo session");

        assert_eq!(reparsed.harness, Harness::Roo);
        assert_eq!(reparsed.messages.len(), 3);
        assert_eq!(reparsed.messages[0].role, Role::User);
        assert_eq!(reparsed.messages[1].role, Role::Assistant);
        // tool_result-only user turn reclassified back to Tool.
        assert_eq!(reparsed.messages[2].role, Role::Tool);

        // cwd + title recovered from the injected <environment_details>/<task> wrappers.
        assert_eq!(reparsed.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(reparsed.title.as_deref(), Some("Add a unit test"));

        // Assistant blocks survived: thinking (with signature), text, tool_use.
        let asst = &reparsed.messages[1];
        assert!(matches!(
            &asst.content[0],
            Block::Thinking { signature: Some(sig), .. } if sig == "SIG=="
        ));
        assert!(asst
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "write_to_file")));

        // Tool result survived with its id + content.
        match &reparsed.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_42");
                assert!(content.contains("File written"));
                assert!(!is_error);
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        let _ = fs::remove_dir_all(&out_dir);
    }
}
