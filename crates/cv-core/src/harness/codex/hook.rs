//! Codex's hook payload (0.163.0, Claude-compatible field names): every thread is its own
//! session, so a sub-agent event's `session_id` and `transcript_path` may be the child's; the
//! owning session is the one the child's rollout header names as its parent. `apply_patch`'s
//! hook input is command-shaped even for a freeform call.

use crate::hooks::{Agent, Event, Payload};
use crate::ir::{Harness, SessionRef};
use anyhow::{Context, Result};
use serde_json::Value;

pub(super) fn hook_event(v: &Value) -> Result<Option<Event>> {
    let p = Payload::decode(v)?;
    let mut transcript = p.transcript();
    let mut session_id = p.session_id.clone();
    let agent = match p.agent_id.clone() {
        None => None,
        Some(id) => {
            let start = p.hook_event_name.as_deref() == Some("SubagentStart");
            let agent_transcript = p
                .agent_transcript()
                .or_else(|| start.then(|| transcript.clone()).flatten());
            if p.is_subagent_event() {
                let path = agent_transcript
                    .as_deref()
                    .context("Codex lane hook has no child transcript")?;
                let parent = super::hook_header(path)?
                    .and_then(|s| s.lineage.parent)
                    .context("Codex child transcript has no parent identity")?;
                if start {
                    transcript = crate::catalog::lookup(&parent, Some(Harness::Codex))
                        .into_iter()
                        .next()
                        .map(|r| r.path);
                }
                session_id = parent;
            }
            Some(Agent {
                id,
                kind: p.agent_type.clone(),
                transcript: agent_transcript,
                last_message: p.last_assistant_message.clone(),
            })
        }
    };
    let edits = p
        .tool_input
        .as_ref()
        .and_then(|tool| tool.get("command"))
        .and_then(Value::as_str)
        .map(patch_paths)
        .unwrap_or_default();
    Ok(Some(Event {
        harness: Harness::Codex,
        session: SessionRef::new(session_id, Harness::Codex, transcript.unwrap_or_default()),
        event: p.hook_event_name,
        cwd: p.cwd,
        agent,
        edits,
    }))
}

/// Every file an `apply_patch` body adds, updates, deletes or moves to, sorted and deduplicated;
/// nothing for a command that is not a patch.
fn patch_paths(patch: &str) -> Vec<String> {
    if patch.trim().lines().next() != Some("*** Begin Patch") {
        return Vec::new();
    }
    let mut paths: Vec<String> = patch
        .trim()
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .into_iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(str::to_string)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_patch_guards_every_destination_and_a_shell_command_is_no_patch() {
        let event = |command: &str| {
            hook_event(&json!({"session_id": "s", "hook_event_name": "PreToolUse", "tool_input": {"command": command}}))
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            event("*** Begin Patch\n*** Update File: from name.rs\n*** Move to: to name.rs\n@@\n-old\n+new\n*** Add File: added.rs\n+hello\n*** Delete File: deleted.rs\n*** End Patch").edits,
            ["added.rs", "deleted.rs", "from name.rs", "to name.rs"]
        );
        assert!(event("echo '*** Update File: pretend.rs'").edits.is_empty());
    }
}
