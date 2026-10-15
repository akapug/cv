//! The existing Anthropic block mapping with checked, bounded text/argument publication.
use super::*;
use crate::{
    harness::wire::write::{self as wire, ObjectWriter},
    lazy::Resolver,
};
use anyhow::Result;
use std::io::Write;
pub(crate) fn block(writer: &mut dyn Write, b: &Block, q: &Quirks, user: bool, resolver: &Resolver) -> Result<()> {
    let mut o = ObjectWriter::new(writer)?;
    match b {
        Block::Text { text } => {
            o.field("type", "text")?;
            o.field_with("text", |w| wire::text(w, text, resolver))?;
        }
        Block::Thinking {
            text,
            signature,
            encrypted,
            redacted,
        } => match q.redacted_thinking {
            Some(t) if *redacted => {
                o.field("type", t)?;
                if let Some(enc) = encrypted {
                    o.field("data", enc)?;
                }
            }
            _ => {
                o.field("type", "thinking")?;
                o.field_with(q.thinking.0, |w| wire::text(w, text, resolver))?;
                if let Some(sig) = signature {
                    o.field(q.thinking.1, sig)?;
                }
            }
        },
        Block::ToolUse { id, name, input, .. } => {
            o.field("type", q.tool_use.0)?;
            o.field("id", id)?;
            o.field("name", name)?;
            o.field_with(q.tool_use.1, |w| input.write_canonical(&mut &mut *w))?;
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            anyhow::ensure!(user, "tool result is a user block");
            o.field("type", "tool_result")?;
            o.field("tool_use_id", tool_use_id)?;
            o.field_with("content", |w| wire::text(w, content, resolver))?;
            o.field("is_error", is_error)?;
        }
        Block::Image { media_type, data_ref } => {
            o.field("type", "image")?;
            if q.flat_media.is_some() {
                if let Some(mt) = media_type {
                    o.field("mimeType", mt)?;
                }
                if let Some(d) = data_ref {
                    o.field("data", d)?;
                }
            } else {
                o.field_with("source", |w| {
                    let mut s = ObjectWriter::new(w)?;
                    if let Some(mt) = media_type {
                        s.field("media_type", mt)?;
                    }
                    match data_ref.as_deref() {
                        Some(r) if r.starts_with("file:") => {
                            s.field("type", "file")?;
                            s.field("file_id", &r[5..])?;
                        }
                        Some("base64:inline") => {
                            s.field("type", "base64")?;
                        }
                        Some(r) => {
                            s.field("type", "url")?;
                            s.field("url", r)?;
                        }
                        None => {}
                    }
                    s.finish()
                })?;
            }
        }
        Block::File { mime, path, source } if user && q.flat_media.is_none() => {
            o.field("type", "document")?;
            if let Some(p) = path {
                o.field("title", p)?;
            }
            o.field_with("source", |w| {
                let mut s = ObjectWriter::new(w)?;
                if let Some(mt) = mime {
                    s.field("media_type", mt)?;
                }
                match source.as_deref() {
                    Some(r) if r.starts_with("file:") => {
                        s.field("type", "file")?;
                        s.field("file_id", &r[5..])?;
                    }
                    Some(r) => {
                        s.field("type", "url")?;
                        s.field("url", r)?;
                    }
                    None => {}
                }
                s.finish()
            })?;
        }
        Block::File { path, source, .. } => {
            o.field("type", "text")?;
            o.field_with("text", |w| {
                crate::harness::store::text::generate_string(w, |builder| {
                    builder.push("[file: ")?;
                    builder.push(path.as_deref().or(source.as_deref()).unwrap_or("?"))?;
                    builder.push("]")
                })
            })?;
        }
    }
    o.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_blocks_match_owned_codec_across_quirks_and_roles() -> Result<()> {
        let content = vec![
            Block::Text {
                text: "a\"\\\nλ".into(),
            },
            Block::Thinking {
                text: "thought".into(),
                signature: Some(String::new()),
                encrypted: None,
                redacted: false,
            },
            Block::Thinking {
                text: String::new().into(),
                signature: None,
                encrypted: Some("opaque".into()),
                redacted: true,
            },
            Block::ToolUse {
                id: "call".into(),
                name: "Read".into(),
                input: json!({"n":1.25,"s":"arg"}).into(),
                namespace: None,
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: "result".into(),
                tool_name: None,
                is_error: true,
                status: None,
                details: None,
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("file:image".into()),
            },
            Block::Image {
                media_type: None,
                data_ref: Some("base64:inline".into()),
            },
            Block::File {
                mime: Some("text/plain".into()),
                path: Some("document".into()),
                source: Some("https://example.invalid/doc".into()),
            },
        ];
        for q in [ANTHROPIC, CLAUDE_AI, OPENCLAW, GOOSE, ZED] {
            for user in [false, true] {
                for b in &content {
                    if let Some(expected) = super::super::block_to(b, &q, user)? {
                        let mut bytes = Vec::new();
                        block(&mut bytes, b, &q, user, &Resolver::new(None))?;
                        assert_eq!(bytes, serde_json::to_vec(&expected)?, "{q:?}, user={user}, block={b:?}");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod source_failure_tests {
    use super::*;
    use crate::harness::store::JsonlStage;
    #[test]
    fn changed_source_during_block_serialization_cannot_publish() -> Result<()> {
        let world = cv_testkit::World::new("anthropic-stage-source-race");
        let source = world.base.join("source.json");
        let destination = world.base.join("destination.jsonl");
        let original = serde_json::to_vec(&"a".repeat(200_000))?;
        let changed = serde_json::to_vec(&"b".repeat(200_000))?;
        std::fs::write(&source, &original)?;
        std::fs::write(&destination, b"existing destination")?;
        let text = crate::Json::source(&source, 0, original.len() as u64)?
            .with_view(|v| v.source_text()?.ok_or_else(|| anyhow::anyhow!("source text")))?;
        struct Mutate<'a> {
            writer: &'a mut dyn Write,
            source: &'a std::path::Path,
            bytes: &'a [u8],
            done: bool,
        }
        impl Write for Mutate<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.done && bytes.len() > 1000 {
                    self.done = true;
                    let mut file = std::fs::OpenOptions::new().write(true).open(self.source)?;
                    file.write_all(self.bytes)?;
                }
                self.writer.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.writer.flush()
            }
        }
        let mut stage = JsonlStage::new();
        assert!(stage
            .push_with(|writer| block(
                &mut Mutate {
                    writer,
                    source: &source,
                    bytes: &changed,
                    done: false
                },
                &Block::Text { text },
                &ANTHROPIC,
                false,
                &Resolver::new(None)
            ))
            .is_err());
        assert!(stage.publish(&destination, crate::publish::Claim::Replace).is_err());
        assert_eq!(std::fs::read(destination)?, b"existing destination");
        Ok(())
    }
}
