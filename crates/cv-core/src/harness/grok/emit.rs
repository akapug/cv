//! The Grok emitter: an IR [`Session`] written into Grok's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::{
    store::JsonlStage,
    wire::write::{self as wire, ObjectWriter},
};
use crate::lazy::{Resolver, Text};
use std::borrow::Cow;

use crate::harness::EmitResult;
use crate::ir::{Block, Harness, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::{json, Map};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Percent-encode everything except unreserved chars, so `/` → `%2F` (matching Grok's dir layout).
const GROK_ENCODE: &AsciiSet = &CONTROLS.add(b'/').add(b' ').add(b'.').add(b'%').add(b':').add(b'\\');

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    crate::emit::validate_native_source(session, Harness::Grok, opts.thinking)?;
    let resolver = session.resolver();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let enc_cwd = utf8_percent_encode(&cwd_str, GROK_ENCODE).to_string();

    let session_dir = out_dir.join(&enc_cwd).join(&new_id);

    let created = session
        .created_at
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // summary.json
    let mut summary = Map::new();
    summary.insert("info".into(), json!({ "id": new_id, "cwd": cwd_str }));
    summary.insert("created_at".into(), json!(created));
    summary.insert("updated_at".into(), json!(updated));
    summary.insert("last_active_at".into(), json!(updated));
    summary.insert("num_chat_messages".into(), json!(session.messages.len()));
    // Real Grok summaries carry these too; without at least `chat_format_version`
    // the loader rejects the dir with "Session does not exist". (Verified live against
    // grok 0.1.219 — adding the full field set is what makes `grok --resume` discover it.)
    let num_turns = session.messages.iter().filter(|m| matches!(m.role, Role::User)).count();
    summary.insert("num_messages".into(), json!(num_turns));
    summary.insert("next_trace_turn".into(), json!(0));
    summary.insert("chat_format_version".into(), json!(1));
    summary.insert("agent_name".into(), json!("grok-build"));
    if let Some(home) = crate::env::var_os("HOME") {
        summary.insert(
            "grok_home".into(),
            json!(format!("{}/.grok", Path::new(&home).to_string_lossy())),
        );
    }
    // Carry the source model verbatim for a Grok→Grok port (it is already a Grok model), and for a
    // cross-harness port only when it *is* a Grok model; otherwise default to `grok-build`. A
    // foreign model id (e.g. `gpt-5.5`) would make Grok replay the history against an incompatible
    // backend, so it is dropped there — but a same-harness rehome must round-trip the model exactly.
    // The verifier asks [`grok_model_id`] the same question, and classifies a pinned foreign model
    // as an expected substitution rather than an avoidable loss.
    summary.insert("current_model_id".into(), json!(grok_model_id(session)));
    if let Some(g) = &session.git {
        if let Some(b) = &g.branch {
            summary.insert("head_branch".into(), json!(b));
        }
        if let Some(c) = &g.commit {
            summary.insert("head_commit".into(), json!(c));
        }
        if let Some(r) = &g.remote {
            summary.insert("git_remotes".into(), json!([r]));
        }
    }
    summary.insert("session_summary".into(), json!(session.title.as_deref().unwrap_or("")));
    let summary_path = session_dir.join("summary.json");

    // chat_history.jsonl, plus an updates.jsonl sidecar carrying each tool call's terminal status.
    // chat_history alone cannot express a failed tool result — the Grok parser derives
    // `ToolResult.is_error`/`status` ONLY from `updates.jsonl` `tool_call_update` entries — so
    // without the sidecar every ported failure came back as a success.
    let mut lines = JsonlStage::new();
    let mut updates = JsonlStage::new();
    for msg in &session.messages {
        match msg.role {
            Role::System | Role::User | Role::Assistant => {
                let (texts, thinking) = projected(msg, opts.thinking, &resolver)?;
                lines.push_with(|writer| {
                    let mut line = ObjectWriter::new(writer)?;
                    line.field(
                        "type",
                        match msg.role {
                            Role::System => "system",
                            Role::User => "user",
                            _ => "assistant",
                        },
                    )?;
                    line.field_with("content", |writer| {
                        if msg.role == Role::User {
                            writer.write_all(b"[")?;
                            let mut block = ObjectWriter::new(writer)?;
                            block.field("type", "text")?;
                            block.field_with("text", |w| {
                                wire::joined_text(w, texts.iter().map(Cow::as_ref), &resolver)
                            })?;
                            block.finish()?;
                            writer.write_all(b"]")?;
                            Ok(())
                        } else {
                            wire::joined_text(writer, texts.iter().map(Cow::as_ref), &resolver)
                        }
                    })?;
                    if msg.role == Role::Assistant {
                        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                            line.field("model_id", model)?;
                        }
                        if let Some(Block::Thinking { text, encrypted, .. }) = thinking {
                            line.field_with("reasoning", |writer| {
                                let mut reason = ObjectWriter::new(writer)?;
                                reason.field_with("text", |w| wire::text(w, text, &resolver))?;
                                if session.harness == Harness::Grok {
                                    if let Some(encrypted) = encrypted {
                                        reason.field("encrypted", encrypted)?;
                                    }
                                }
                                reason.finish()
                            })?;
                        }
                        if msg.content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
                            line.field_with("tool_calls", |writer| {
                                writer.write_all(b"[")?;
                                let mut first = true;
                                for block in &msg.content {
                                    if let Block::ToolUse { id, name, input, .. } = block {
                                        if !first {
                                            writer.write_all(b",")?;
                                        }
                                        first = false;
                                        let mut call = ObjectWriter::new(writer)?;
                                        call.field("id", id)?;
                                        call.field("name", name)?;
                                        call.field_with("arguments", |w| {
                                            crate::harness::store::text::generate_string(w, |builder| {
                                                input.with_view(|view| builder.push_json(view))
                                            })
                                        })?;
                                        call.finish()?;
                                    }
                                }
                                writer.write_all(b"]")?;
                                Ok(())
                            })?;
                        }
                    }
                    line.finish()
                })?;
            }
            Role::Tool => {
                for block in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        status,
                        ..
                    } = block
                    {
                        lines.push_with(|writer| {
                            let mut line = ObjectWriter::new(writer)?;
                            line.field("type", "tool_result")?;
                            line.field("tool_call_id", tool_use_id)?;
                            line.field_with("content", |w| wire::text(w, content, &resolver))?;
                            line.finish()
                        })?;
                        updates.push(json!({
                            "timestamp": epoch_ms(msg.timestamp.or(session.updated_at)),
                            "method": "session/update",
                            "params": {
                                "sessionId": new_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": tool_use_id,
                                    "status": status.as_deref().unwrap_or(if *is_error { "failed" } else { "completed" }),
                                },
                            },
                        }))?;
                    }
                }
            }
        }
    }
    // Checked source production completes before any destination file or directory is touched.
    fs::create_dir_all(&session_dir).with_context(|| format!("creating {}", session_dir.display()))?;
    let summary = serde_json::to_string_pretty(&summary)?;
    crate::publish::publish_reader(&summary_path, "stage", summary.as_bytes(), opts.claim())?;
    let chat_path = session_dir.join("chat_history.jsonl");
    lines.publish(&chat_path, opts.claim())?;
    if !updates.is_empty() {
        updates.publish(&session_dir.join("updates.jsonl"), opts.claim())?;
    }

    Ok(EmitResult {
        path: session_dir,
        new_id: new_id.clone(),
        resume_hint: Some(format!("grok --resume {new_id}")),
    })
}

/// Project each block once; retained text and the first retained thinking stay borrowed.
fn projected<'a>(
    message: &'a crate::ir::Message,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<(Vec<Cow<'a, Text>>, Option<&'a Block>)> {
    let mut texts = Vec::new();
    let mut thinking = None;
    for block in &message.content {
        let fate = crate::emit::thinking::fate(block, Harness::Grok, mode, resolver)?;
        if let Block::Text { text } = block {
            texts.push(Cow::Borrowed(text));
        } else if let Some(text) = crate::emit::thinking::text_with_fate(block, fate) {
            texts.push(text);
        } else if thinking.is_none() && fate == ThinkingFate::Keep && matches!(block, Block::Thinking { .. }) {
            thinking = Some(block);
        }
    }
    Ok((texts, thinking))
}

/// The model id [`emit_grok`] records in `summary.json` as `current_model_id` — the session's own
/// when Grok can serve it, else [`GROK_FALLBACK_MODEL`]. Shared with the verifier so "did the
/// emitter pin a foreign model?" is asked of the same code that does the pinning.
pub(crate) fn grok_model_id(session: &Session) -> &str {
    session
        .model
        .as_deref()
        .filter(|m| session.harness == Harness::Grok || m.to_ascii_lowercase().contains("grok"))
        .unwrap_or(GROK_FALLBACK_MODEL)
}

/// What `summary.json` records when the source model is not one Grok can run.
pub(crate) const GROK_FALLBACK_MODEL: &str = "grok-build";
