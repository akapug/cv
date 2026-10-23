//! Cline and Roo's existing Anthropic history schema, streamed from checked IR leaves.
use super::*;
use crate::{
    emit::{EmitOptions, ThinkingFate},
    harness::{store::bytes::ByteStage, wire::write::ObjectWriter},
    ir_json::PrettyJson,
    lazy::Resolver,
};
use std::io::{BufWriter, Write};

#[cfg(test)]
mod tests;

fn text_block(writer: &mut dyn Write, parts: &[&str]) -> Result<()> {
    let mut block = ObjectWriter::new(writer)?;
    block.field("type", "text")?;
    block.field_with("text", |w| {
        crate::harness::store::text::generate_string(w, |b| {
            for part in parts {
                b.push(part)?;
            }
            Ok(())
        })
    })?;
    block.finish()
}

/// The write-side facts every content array of one history shares.
struct Context<'a> {
    session: &'a Session,
    opts: &'a EmitOptions,
    harness: Harness,
    resolver: &'a Resolver,
}

fn blocks(writer: &mut dyn Write, message: &Message, c: &Context<'_>, cwd: Option<&str>) -> Result<()> {
    writer.write_all(b"[")?;
    let mut first = true;
    let mut separate = |w: &mut dyn Write| -> Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        Ok(())
    };
    if let Some(title) = cwd.and(c.session.title.as_deref()) {
        separate(writer)?;
        text_block(writer, &["<task>", title, "</task>"])?;
    }
    let user = message.role == Role::User || message.role == Role::Tool;
    for block in &message.content {
        if message.role == Role::Tool && !matches!(block, Block::ToolResult { .. }) {
            continue;
        }
        // Tool results belong to Tool rows; one on an assistant or system row has no
        // representable slot in this history schema, so it is skipped rather than misfiled.
        if !user && matches!(block, Block::ToolResult { .. }) {
            continue;
        }
        let fate = crate::emit::thinking::fate(block, c.harness, c.opts.thinking, c.resolver)?;
        if fate == ThinkingFate::Drop {
            continue;
        }
        separate(writer)?;
        if let Some(text) = crate::emit::thinking::text_with_fate(block, fate) {
            let mut projected = ObjectWriter::new(writer)?;
            projected.field("type", "text")?;
            projected.field_with("text", |w| crate::harness::wire::write::text(w, &text, c.resolver))?;
            projected.finish()?;
        } else {
            anthropic::write::block(writer, block, &anthropic::ANTHROPIC, user, c.resolver)?;
        }
    }
    // The first user turn carries cwd even without a title; only the optional task wrapper
    // above depends on a title. The reader recovers cwd from this environment trailer.
    if let Some(cwd) = cwd {
        separate(writer)?;
        text_block(
            writer,
            &[
                "<environment_details>\n# Current Working Directory (",
                cwd,
                ") Files\n</environment_details>",
            ],
        )?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

pub(super) fn history(session: &Session, opts: &EmitOptions, harness: Harness, cwd: Option<&str>) -> Result<ByteStage> {
    crate::emit::validate_native_source(session, harness, opts.thinking)?;
    let resolver = session.resolver();
    let c = Context {
        session,
        opts,
        harness,
        resolver: &resolver,
    };
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut pretty = PrettyJson::new(&mut stage, 0);
        pretty.write_all(b"[")?;
        let mut first = true;
        let mut first_user = true;
        for message in &session.messages {
            if !first {
                pretty.write_all(b",")?;
            }
            first = false;
            let inject = if message.role == Role::User && first_user {
                first_user = false;
                cwd
            } else {
                None
            };
            let role = match message.role {
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::User | Role::Tool => "user",
            };
            let mut row = ObjectWriter::new(&mut pretty)?;
            row.field("role", role)?;
            row.field_with("content", |w| blocks(w, message, &c, inject))?;
            row.finish()?;
        }
        pretty.write_all(b"]")?;
        pretty.flush()?;
    }
    Ok(stage.into_inner().map_err(|e| e.into_error())?)
}
