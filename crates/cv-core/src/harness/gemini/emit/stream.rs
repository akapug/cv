//! Borrow the existing IR, stream checked leaves, and preserve the legacy ordered pretty wire.
use super::*;
use crate::harness::wire::genai;
use crate::{
    harness::{
        store::{bytes::ByteStage, stage_into},
        wire::write::{self, ObjectWriter},
    },
    ir::Message,
    ir_json::PrettyJson,
    lazy::{Resolver, Text},
    Harness,
};
use std::{
    collections::HashMap,
    io::{BufWriter, Write},
};

type Results<'a> = HashMap<&'a str, (&'a Text, bool)>;

fn parts(writer: &mut dyn Write, message: &Message, fates: &[ThinkingFate], resolver: &Resolver) -> Result<()> {
    writer.write_all(b"[")?;
    let mut first = true;
    for (block, &projection) in message.content.iter().zip(fates) {
        if matches!(block, Block::ToolUse { .. } | Block::ToolResult { .. }) {
            continue;
        }
        if projection == ThinkingFate::Drop {
            continue;
        }
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let mut object = ObjectWriter::new(writer)?;
        match block {
            Block::Text { text } => genai::text_fields(&mut object, text, false, None, resolver)?,
            Block::Thinking {
                text,
                signature,
                encrypted,
                redacted,
            } => {
                if projection == ThinkingFate::Placeholder {
                    object.field(
                        "text",
                        thinking_placeholder(signature.as_deref(), encrypted.as_deref(), *redacted),
                    )?;
                } else {
                    genai::text_fields(&mut object, text, projection == ThinkingFate::Keep, None, resolver)?;
                }
            }
            block @ (Block::Image { .. } | Block::File { .. }) => genai::media_fields(&mut object, block)?,
            Block::ToolUse { .. } | Block::ToolResult { .. } => unreachable!(),
        }
        object.finish()?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn response(writer: &mut dyn Write, id: &str, name: &str, text: &Text, resolver: &Resolver) -> Result<()> {
    writer.write_all(b"[")?;
    let mut outer = ObjectWriter::new(writer)?;
    outer.field_with("functionResponse", |w| {
        let mut response = ObjectWriter::new(w)?;
        response.field("id", id)?;
        response.field("name", name)?;
        response.field_with("response", |w| {
            let mut output = ObjectWriter::new(w)?;
            output.field_with("output", |w| write::text(w, text, resolver))?;
            output.finish()
        })?;
        response.finish()
    })?;
    outer.finish()?;
    writer.write_all(b"]")?;
    Ok(())
}

fn calls(
    writer: &mut dyn Write,
    message: &Message,
    results: &Results<'_>,
    called: &std::collections::HashSet<&str>,
    resolver: &Resolver,
) -> Result<()> {
    writer.write_all(b"[")?;
    let mut first = true;
    for block in &message.content {
        match block {
            Block::ToolUse { id, name, input, .. } if message.role == Role::Assistant => {
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                let mut call = ObjectWriter::new(writer)?;
                call.field("id", id)?;
                call.field("name", name)?;
                call.field_with("args", |w| input.write_canonical(&mut &mut *w))?;
                if let Some((text, error)) = results.get(id.as_str()) {
                    call.field("status", if *error { "error" } else { "success" })?;
                    call.field_with("result", |w| response(w, id, name, text, resolver))?;
                }
                call.finish()?;
            }
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                tool_name,
                ..
            } if message.role == Role::Tool && !called.contains(tool_use_id.as_str()) => {
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                let name = tool_name.as_deref().unwrap_or("tool");
                let mut call = ObjectWriter::new(writer)?;
                call.field("id", tool_use_id)?;
                call.field("name", name)?;
                call.field("status", if *is_error { "error" } else { "success" })?;
                call.field_with("result", |w| response(w, tool_use_id, name, content, resolver))?;
                call.finish()?;
            }
            _ => {}
        }
    }
    writer.write_all(b"]")?;
    Ok(())
}

/// A message's id: name-based (UUID v5) from the session id and the message's index in the IR,
/// so emitting one session twice under one id writes the same ids.
fn message_id(session_id: &str, index: usize) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("urn:cv:gemini:{session_id}:{index}").as_bytes(),
    )
}

fn messages(
    writer: &mut dyn Write,
    session: &Session,
    session_id: &str,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<()> {
    let mut results = Results::new();
    for message in &session.messages {
        if message.role == Role::Tool {
            for block in &message.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = block
                {
                    results.insert(tool_use_id.as_str(), (content, *is_error));
                }
            }
        }
    }
    let called = called_tool_ids(session);
    writer.write_all(b"[")?;
    let mut first = true;
    for (index, message) in session.messages.iter().enumerate() {
        let has_calls = message.content.iter().any(|block| match block {
            Block::ToolUse { .. } => message.role == Role::Assistant,
            Block::ToolResult { tool_use_id, .. } => {
                message.role == Role::Tool && !called.contains(tool_use_id.as_str())
            }
            _ => false,
        });
        if message.role == Role::Tool && !has_calls {
            continue;
        }
        let fates: Vec<_> = if message.role == Role::Tool {
            Vec::new()
        } else {
            message
                .content
                .iter()
                .map(|block| crate::emit::thinking::fate(block, Harness::Gemini, mode, resolver))
                .collect::<Result<_>>()?
        };
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let mut record = ObjectWriter::new(writer)?;
        record.field("id", message_id(session_id, index).to_string())?;
        record.field(
            "type",
            match message.role {
                Role::User => "user",
                Role::System => "info",
                _ => "gemini",
            },
        )?;
        if let Some(ts) = message.timestamp {
            record.field("timestamp", ts.to_rfc3339_opts(SecondsFormat::Millis, true))?;
        }
        if message.role != Role::Tool {
            if message.role == Role::Assistant {
                if let Some(model) = message.model.as_ref().or(session.model.as_ref()) {
                    record.field("model", model)?;
                }
                if let Some(usage) = &message.usage {
                    let mut tokens = Map::new();
                    if let Some(input) = usage.input_tokens {
                        tokens.insert("input".into(), json!(input + usage.cache_read_tokens.unwrap_or(0)));
                    }
                    if let Some(output) = usage.output_tokens {
                        tokens.insert("output".into(), json!(output));
                    }
                    if let Some(cached) = usage.cache_read_tokens {
                        tokens.insert("cached".into(), json!(cached));
                    }
                    if !tokens.is_empty() {
                        record.field("tokens", tokens)?;
                    }
                }
                let mut thoughts = Vec::new();
                for (block, &projection) in message.content.iter().zip(&fates) {
                    if matches!(block, Block::Thinking { .. }) && projection == ThinkingFate::Keep {
                        thoughts.push(block);
                    }
                }
                if !thoughts.is_empty() {
                    record.field_with("thoughts", |w| {
                        w.write_all(b"[")?;
                        for (i, block) in thoughts.iter().enumerate() {
                            if i != 0 {
                                w.write_all(b",")?;
                            }
                            let Block::Thinking { text, .. } = block else {
                                unreachable!()
                            };
                            let mut thought = ObjectWriter::new(w)?;
                            thought.field("subject", "")?;
                            thought.field_with("description", |w| write::text(w, text, resolver))?;
                            thought.finish()?;
                        }
                        w.write_all(b"]")?;
                        Ok(())
                    })?;
                }
            }
            record.field_with("content", |w| parts(w, message, &fates, resolver))?;
        }
        if has_calls {
            record.field_with("toolCalls", |w| calls(w, message, &results, &called, resolver))?;
        }
        record.finish()?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

pub(super) fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    crate::emit::validate_native_source(session, Harness::Gemini, opts.thinking)?;
    let resolver = session.resolver();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let start = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let last = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut pretty = PrettyJson::new(&mut stage, 0);
        let mut record = ObjectWriter::new(&mut pretty)?;
        record.field("sessionId", &new_id)?;
        record.field("projectHash", short_hash(&cwd_str))?;
        record.field("startTime", start)?;
        record.field("lastUpdated", last)?;
        if let Some(title) = &session.title {
            record.field("summary", title)?;
        }
        if let Some(cwd) = &cwd {
            record.field("directories", [cwd.to_string_lossy()])?;
        }
        record.field_with("messages", |w| messages(w, session, &new_id, opts.thinking, &resolver))?;
        record.finish()?;
    }
    // Checked serialization completes before anything is published. The transcript and its
    // project registration publish as one set, the transcript first: a crash between them leaves
    // a session Gemini re-registers from its directory, never a registration with no session.
    let stage = stage.into_inner().map_err(|error| error.into_error())?;
    let slug = project_slug(out_dir, &cwd_str);
    let chats = match &slug {
        Some(slug) => out_dir.join(slug).join("chats"),
        None => out_dir.join("chats"),
    };
    fs::create_dir_all(&chats).with_context(|| format!("creating {}", chats.display()))?;
    let path = chats.join(format!("session-{new_id}.json"));
    let mut set = crate::publish::Set::new();
    stage_into(&mut set, stage, &path, opts.claim())?;
    if let Some(slug) = &slug {
        stage_project(out_dir, slug, &cwd_str, &mut set)?;
    }
    set.publish()?;
    Ok(EmitResult {
        path,
        new_id: new_id.clone(),
        resume_hint: Some(match cwd {
            Some(cwd) => format!("gemini --resume {new_id}  (run from {})", cwd.display()),
            None => format!("gemini --resume {new_id}"),
        }),
    })
}
