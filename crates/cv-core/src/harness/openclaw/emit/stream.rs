//! Existing v3 records, written from guarded leaves without payload-bearing intermediate Values.
use super::*;
use crate::{
    harness::wire::write::{self as wire, ObjectWriter},
    lazy::Resolver,
};
use std::io::Write;

pub(super) fn content(writer: &mut dyn Write, blocks: &[Block], mode: ThinkingMode, resolver: &Resolver) -> Result<()> {
    writer.write_all(b"[")?;
    let mut first = true;
    for block in blocks {
        if matches!(block, Block::ToolResult { .. }) {
            continue;
        }
        let fate = crate::emit::thinking::fate(block, Harness::OpenClaw, mode, resolver)?;
        if fate == ThinkingFate::Drop {
            continue;
        }
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        if let Some(text) = crate::emit::thinking::text_with_fate(block, fate) {
            let mut projected = ObjectWriter::new(writer)?;
            projected.field("type", "text")?;
            projected.field_with("text", |w| wire::text(w, &text, resolver))?;
            projected.finish()?;
        } else {
            match block {
                Block::ToolUse {
                    id,
                    name,
                    input,
                    namespace: Some(namespace),
                } if namespace == crate::affordance::DEFERRED => {
                    // The adapter's own wrapper supplies OpenClaw's distinct {id,args} keys.
                    let crate::affordance::DeferredWrapper { name_key, args_key } =
                        crate::harness::openclaw::DEFERRED_WRAPPER;
                    let mut call = ObjectWriter::new(writer)?;
                    call.field("type", anthropic::OPENCLAW.tool_use.0)?;
                    call.field("id", id)?;
                    call.field("name", crate::affordance::DEFERRED)?;
                    call.field_with(anthropic::OPENCLAW.tool_use.1, |w| {
                        let mut wrapper = ObjectWriter::new(w)?;
                        wrapper.field(name_key, name)?;
                        wrapper.field_with(args_key, |w| input.write_canonical(&mut &mut *w))?;
                        wrapper.finish()
                    })?;
                    call.finish()?;
                }
                _ => anthropic::write::block(writer, block, &anthropic::OPENCLAW, false, resolver)?,
            }
        }
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn summary(writer: &mut dyn Write, msg: &crate::Message, mode: ThinkingMode, resolver: &Resolver) -> Result<()> {
    let mut texts = Vec::new();
    for block in &msg.content {
        if let Some(text) = crate::emit::thinking::text(block, Harness::OpenClaw, mode, resolver)? {
            texts.push(text);
        }
    }
    wire::joined_text(writer, texts.iter().map(|v| v.as_ref()), resolver)
}

fn tool_result(writer: &mut dyn Write, blocks: &[Block], timestamp: i64, resolver: &Resolver) -> Result<()> {
    let result = blocks.iter().find_map(|block| {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            details,
            ..
        } = block
        {
            Some((
                tool_use_id.as_str(),
                content,
                *is_error,
                tool_name.as_deref(),
                details.as_ref(),
            ))
        } else {
            None
        }
    });
    let mut inner = ObjectWriter::new(writer)?;
    inner.field("role", "toolResult")?;
    inner.field("toolCallId", result.map(|r| r.0).unwrap_or(""))?;
    if let Some(name) = result.and_then(|r| r.3) {
        inner.field("toolName", name)?;
    }
    inner.field_with("content", |w| {
        w.write_all(b"[")?;
        let mut part = ObjectWriter::new(w)?;
        part.field("type", "text")?;
        part.field_with("text", |w| match result {
            Some((_, text, _, _, _)) => wire::text(w, text, resolver),
            None => serde_json::to_writer(w, "").map_err(Into::into),
        })?;
        part.finish()?;
        for block in blocks.iter().filter(|block| matches!(block, Block::Image { .. })) {
            w.write_all(b",")?;
            anthropic::write::block(w, block, &anthropic::OPENCLAW, false, resolver)?;
        }
        w.write_all(b"]")?;
        Ok(())
    })?;
    inner.field("isError", result.is_some_and(|r| r.2))?;
    if let Some(details) = result.and_then(|r| r.4) {
        inner.field("details", details)?;
    }
    inner.field("timestamp", timestamp)?;
    inner.finish()
}

pub(super) fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    emit_after_stage(session, out_dir, opts, || Ok(()))
}

pub(super) fn emit_after_stage(
    session: &Session,
    out_dir: &Path,
    opts: &EmitOptions,
    after_stage: impl FnOnce() -> Result<()>,
) -> Result<EmitResult> {
    wire::check_source_fields(session)?;
    let resolver = session.resolver();
    // Match explicit-title precedence, first-user selection, whitespace filtering and truncation.
    let label = wire::fallback_title(session, &resolver)?;
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let agents_root = if out_dir.file_name().is_some_and(|n| n == "agents") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("agents")
    };
    let sessions_dir = agents_root.join("main/sessions");
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut lines = JsonlStage::new();
    let native = super::replay::Native::new(session, &new_id);
    lines.push_with(|w| {
        if native.header(session, w, &new_id, &cwd_str)? {
            return Ok(());
        }
        let mut header = ObjectWriter::new(w)?;
        header.field("type", "session")?;
        header.field("version", 3)?;
        header.field("id", &new_id)?;
        header.field("timestamp", &created_iso)?;
        if let Some(parent) = &session.lineage.forked_from {
            header.field("parentSession", parent)?;
        }
        if !cwd_str.is_empty() {
            header.field("cwd", &cwd_str)?;
        }
        header.finish()
    })?;
    if let Some(title) = session
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty() && !native.has_title_record(session))
    {
        lines.push_with(|w| {
            let mut row = ObjectWriter::new(w)?;
            row.field("type", "session_info")?;
            row.field("id", native.generated_id("title"))?;
            row.field("parentId", Value::Null)?;
            row.field("timestamp", &created_iso)?;
            row.field("name", title)?;
            row.finish()
        })?;
    }
    let mut parent: Option<String> = None;
    let mut index = 0;
    while index < session.messages.len() {
        let msg = &session.messages[index];
        let paired = msg.kind == MessageKind::CompactionBoundary
            && msg.id.is_some()
            && session
                .messages
                .get(index + 1)
                .is_some_and(|next| next.kind == MessageKind::CompactionSummary && next.parent_id == msg.id);
        let summary_message = if paired { &session.messages[index + 1] } else { msg };
        let mut consumed = if paired { 2 } else { 1 };
        let ts = msg.timestamp.unwrap_or(created);
        let control = match msg.kind {
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary => Some("compaction"),
            MessageKind::Branch => Some("reset"),
            MessageKind::ModelChange => Some("model_change"),
            _ => None,
        };
        let mut remaining = msg.content.as_slice();
        let mut first_part = true;
        let mut part_index = 0;
        loop {
            let split = if msg.role == Role::Tool && control.is_none() {
                remaining
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| matches!(block, Block::ToolResult { .. }))
                    .nth(1)
                    .map(|(i, _)| i)
            } else {
                None
            };
            let (blocks, rest) = if let Some(split) = split {
                let (a, b) = remaining.split_at(split);
                (a, Some(b))
            } else {
                (remaining, None)
            };
            let entry_id = if first_part {
                native.entry_id(msg, index)
            } else {
                native.generated_id(&format!("part:{index}:{part_index}"))
            };
            let row_parent = if first_part {
                native.parent(msg, &parent)
            } else {
                parent.clone()
            };
            let mut replayed = false;
            lines.push_with(|w| {
                if first_part {
                    if let Some(group) = native.record(&session.messages[index..], w, opts.thinking, &resolver)? {
                        consumed = group;
                        replayed = true;
                        return Ok(());
                    }
                }
                let mut row = ObjectWriter::new(w)?;
                row.field("type", control.unwrap_or("message"))?;
                row.field("id", &entry_id)?;
                row.field("parentId", &row_parent)?;
                row.field("timestamp", ts.to_rfc3339_opts(SecondsFormat::Millis, true))?;
                match control {
                    Some("compaction") => {
                        if msg.kind == MessageKind::CompactionSummary || paired {
                            row.field_with("summary", |w| summary(w, summary_message, opts.thinking, &resolver))?;
                        }
                    }
                    Some("reset") => row.field("reason", "reset")?,
                    Some("model_change") => {
                        row.field("provider", Value::Null)?;
                        row.field("modelId", msg.model.as_ref().or(session.model.as_ref()))?;
                    }
                    _ => row.field_with("message", |w| {
                        if msg.role == Role::Tool {
                            return tool_result(w, blocks, ts.timestamp_millis(), &resolver);
                        }
                        let mut inner = ObjectWriter::new(w)?;
                        inner.field(
                            "role",
                            match msg.role {
                                Role::User => "user",
                                Role::System => "system",
                                _ => "assistant",
                            },
                        )?;
                        inner.field_with("content", |w| content(w, &msg.content, opts.thinking, &resolver))?;
                        inner.field("timestamp", ts.timestamp_millis())?;
                        if msg.role == Role::Assistant {
                            if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                                inner.field("model", model)?;
                            }
                            if let Some(usage) = openclaw_usage(msg.usage.as_ref()) {
                                inner.field("usage", usage)?;
                            }
                        }
                        inner.finish()
                    })?,
                }
                row.finish()
            })?;
            // Id-less replayed records retain their own links and do not introduce an outer
            // entry id to parent the next message; generated or identified records do.
            if !replayed || msg.id.is_some() {
                parent = Some(entry_id);
            }
            if replayed {
                break;
            }
            let Some(rest) = rest else { break };
            remaining = rest;
            first_part = false;
            part_index += 1;
        }
        index += consumed;
    }
    // All source demands complete before opening the transcript or changing its index.
    fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
    let index_path = sessions_dir.join("sessions.json");
    // cv's update replaces OpenClaw's index whole, so an index it cannot read as one is refused,
    // never replaced by one holding only this session.
    let refused = || {
        format!(
            "reading the OpenClaw index {}; refusing to replace it",
            index_path.display()
        )
    };
    // The index is claimed as read: the file this update was computed from, or none.
    let (read, mut index): (Option<crate::source::FileId>, Map<String, Value>) = match fs::File::open(&index_path) {
        Ok(mut file) => {
            let id = crate::source::FileId::of(&file.metadata().with_context(refused)?);
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes).with_context(refused)?;
            (id, serde_json::from_slice(&bytes).with_context(refused)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, Map::new()),
        Err(e) => return Err(e).with_context(refused),
    };
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(label) = label {
        entry.insert("label".into(), json!(label));
    }
    if !cwd_str.is_empty() {
        entry.insert("cwd".into(), json!(cwd_str));
    }
    entry.insert(
        "updatedAt".into(),
        json!(epoch_ms(session.updated_at.or(session.created_at))),
    );
    index.insert(new_id.clone(), Value::Object(entry));
    let mut index_stage = crate::harness::store::bytes::ByteStage::new();
    serde_json::to_writer_pretty(&mut index_stage, &index)?;
    // The transcript and its index entry publish as one set, the index last: a crash between
    // leaves an unlisted transcript, never an entry naming a missing one.
    let mut set = crate::publish::Set::new();
    lines.stage_into(&mut set, &file_path, opts.claim())?;
    crate::harness::store::stage_into(&mut set, index_stage, &index_path, crate::publish::Claim::Replace)?;
    after_stage()?;
    // Another writer's index update since the read would be lost under ours: refuse, publish nothing.
    if crate::source::FileId::at(&index_path) != read {
        anyhow::bail!(
            "the OpenClaw index {} changed while cv updated it; rerun",
            index_path.display()
        );
    }
    set.publish()?;
    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}
