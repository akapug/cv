//! The Anthropic Messages content shape — `text | thinking | redacted_thinking | tool_use |
//! tool_result | image | document` — read into [`Block`]s and written back. Claude Code and Cline
//! persist it verbatim; the claude.ai export, OpenClaw, Goose and Zed's legacy threads renamed a
//! key or two, which a [`Quirks`] constant names so the adapter does not re-type the shape.

use super::{coerce_text, str_field};
pub(crate) mod write;
use crate::ir::Block;
use crate::lazy::Text;
use serde_json::{json, Map, Value};

/// How a store spells the shape. Every field is the Anthropic spelling in [`ANTHROPIC`].
#[derive(Debug, Clone, Copy)]
pub struct Quirks {
    /// The tool-invocation block's `type` and its input key: `("tool_use", "input")`; OpenClaw
    /// `("toolCall", "arguments")`.
    pub tool_use: (&'static str, &'static str),
    /// The thinking block's text key and signature key: `("thinking", "signature")`; OpenClaw
    /// `("thinking", "thinkingSignature")`; Zed `("text", "signature")`.
    pub thinking: (&'static str, &'static str),
    /// The redacted-thinking block's `type`: `redacted_thinking`; Goose `redactedThinking`; Zed
    /// `RedactedThinking`. `None` for a store without one — on the way out a redacted block is then
    /// written as a plain thinking block.
    pub redacted_thinking: Option<&'static str>,
    /// An image's or document's payload: Anthropic's `source {type, media_type, data|url|file_id}`
    /// object (`None`), or flat `mimeType`/`data`/`name` fields with `data` kept as a reference
    /// truncated to this many chars (OpenClaw 80, Goose 120).
    pub flat_media: Option<usize>,
    /// Drop a `text` block whose text is empty rather than keep it (Zed, the claude.ai export).
    pub skip_empty_text: bool,
    /// An empty signature string means no signature (Goose, Zed), not a present-but-empty one.
    pub empty_signature_is_none: bool,
}

/// The shape as Anthropic defines it: Claude Code, Cline, Roo.
pub const ANTHROPIC: Quirks = Quirks {
    tool_use: ("tool_use", "input"),
    thinking: ("thinking", "signature"),
    redacted_thinking: Some("redacted_thinking"),
    flat_media: None,
    skip_empty_text: false,
    empty_signature_is_none: false,
};
/// The claude.ai account export: Anthropic's shape with empty text blocks dropped.
pub const CLAUDE_AI: Quirks = Quirks {
    skip_empty_text: true,
    ..ANTHROPIC
};
/// OpenClaw: `toolCall`/`arguments`, `thinkingSignature`, flat `mimeType`/`data` images.
pub const OPENCLAW: Quirks = Quirks {
    tool_use: ("toolCall", "arguments"),
    thinking: ("thinking", "thinkingSignature"),
    redacted_thinking: None,
    flat_media: Some(80),
    ..ANTHROPIC
};
/// Goose: `redactedThinking`, flat `mimeType`/`data`/`name` media, empty signatures absent.
pub const GOOSE: Quirks = Quirks {
    redacted_thinking: Some("redactedThinking"),
    flat_media: Some(120),
    empty_signature_is_none: true,
    ..ANTHROPIC
};
/// Zed's legacy (0.1.0) thread segments: thinking text under `text`, serde-tagged `RedactedThinking`.
pub const ZED: Quirks = Quirks {
    thinking: ("text", "signature"),
    redacted_thinking: Some("RedactedThinking"),
    skip_empty_text: true,
    empty_signature_is_none: true,
    ..ANTHROPIC
};

/// A content string's [`Text`], given the string and the key it came from. The default makes an
/// inline copy; Claude's lazy reader supplies one that spans the source bytes instead.
pub type Texter<'a> = &'a mut dyn FnMut(&str, &'static str) -> Text;

/// One content block → one [`Block`]; `None` for a type the IR has no home for.
pub fn block_from(item: &Value, q: &Quirks) -> Option<Block> {
    block_from_with(item, q, &mut |s, _| Text::Inline(s.to_string()))
}

/// [`block_from`] with the caller choosing how each content string becomes a [`Text`]: `text` is
/// called with the string and its key (`text`, `thinking`, `content`) for every field a span could
/// replace — a tool result's `content` only when it is a plain string (array content is coerced and
/// can never be a verbatim slice).
pub fn block_from_with(item: &Value, q: &Quirks, text: Texter<'_>) -> Option<Block> {
    block_from_fields(item, q, &mut |key| {
        let value = item.get(key).and_then(Value::as_str);
        if item.get("type").and_then(Value::as_str) == Some("thinking") && key == q.thinking.0 {
            Some(text(value.unwrap_or(""), key))
        } else {
            value.map(|s| text(s, key))
        }
    })
}

fn block_from_fields(item: &Value, q: &Quirks, text: &mut dyn FnMut(&'static str) -> Option<Text>) -> Option<Block> {
    let ty = item.get("type").and_then(Value::as_str)?;
    if ty == "text" {
        let value = text("text")?;
        if q.skip_empty_text && value.is_empty() {
            return None;
        }
        return Some(Block::Text { text: value });
    }
    if ty == "thinking" {
        let (text_key, sig_key) = q.thinking;
        let value = text(text_key).unwrap_or_default();
        let signature = item
            .get(sig_key)
            .and_then(Value::as_str)
            .filter(|s| !(q.empty_signature_is_none && s.is_empty()))
            .map(str::to_string);
        return Some(Block::Thinking {
            text: value,
            signature,
            encrypted: None,
            redacted: false,
        });
    }
    // Encrypted reasoning the API returns when thinking can't be shown in the clear. The plaintext
    // is intentionally absent; the opaque blob rides in `encrypted` so it is not silently lost.
    if Some(ty) == q.redacted_thinking {
        return Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: item.get("data").and_then(Value::as_str).map(str::to_string),
            redacted: true,
        });
    }
    if ty == q.tool_use.0 || matches!(ty, "image" | "document") {
        // These shapes do not use the caller's text callback. Owned entry points use the
        // same field selection and conversion as the source-backed codec.
        return block_from_view_with(item.into(), q, false, &mut |v| v.text(false), u64::MAX)
            .expect("an owned JSON value has no source reads");
    }
    match ty {
        "tool_result" => {
            let cv = item.get("content");
            let content = text("content").unwrap_or_else(|| Text::Inline(coerce_text(cv)));
            Some(Block::ToolResult {
                tool_use_id: str_field(item, "tool_use_id"),
                content,
                is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                tool_name: None,
                status: None,
                details: None,
            })
        }
        _ => None,
    }
}

/// A content block from a view, with the caller's text rule for content strings, `spans` for tool
/// arguments, and the size above which a source-backed tool-result array becomes checked joined
/// text rather than being coerced inline. Unselected fields are never decoded, in every demand.
pub(crate) fn block_from_view_with(
    view: crate::json::JsonView<'_>,
    q: &Quirks,
    spans: bool,
    text: &mut dyn FnMut(crate::json::JsonView<'_>) -> anyhow::Result<Option<Text>>,
    joined_over: u64,
) -> anyhow::Result<Option<Block>> {
    use super::super::store::document::Fields;
    let keys = [
        "type",
        "source",
        "title",
        "mimeType",
        "id",
        "name",
        "data",
        "tool_use_id",
        "is_error",
        "content",
        "text",
        q.thinking.0,
        q.thinking.1,
        q.tool_use.1,
    ];
    let Some(record) = Fields::parse(view, &keys)? else {
        return Ok(None);
    };
    let Some(ty) = record.string("type")? else {
        return Ok(None);
    };
    if ty == q.tool_use.0 {
        return Ok(Some(Block::ToolUse {
            id: record.string("id")?.unwrap_or_default(),
            name: record.string("name")?.unwrap_or_default(),
            input: record
                .get(q.tool_use.1)
                .map(|v| v.argument(spans))
                .transpose()?
                .unwrap_or_default(),
            namespace: None,
        }));
    }
    if matches!(ty.as_str(), "image" | "document") {
        if let Some(max) = q.flat_media {
            let mime = record.string("mimeType")?;
            let data = match record.get("data") {
                Some(data) if data.is_string()? => Some(data),
                _ => None,
            };
            return Ok(Some(if ty == "image" {
                let data_ref = data
                    .map(|v| -> anyhow::Result<_> {
                        let text = v.source_text()?.unwrap();
                        let (head, _) = super::super::store::document::text_head(
                            &text,
                            &crate::lazy::Resolver::new(None),
                            max.saturating_add(1),
                            false,
                        )?;
                        Ok(crate::ir::truncate(&head, max))
                    })
                    .transpose()?;
                Block::Image {
                    media_type: mime,
                    data_ref,
                }
            } else {
                Block::File {
                    mime,
                    path: record.string("name")?,
                    source: data.map(|_| "base64:inline".into()),
                }
            }));
        }
        let src = record
            .get("source")
            .map(|v| Fields::parse(v, &["type", "media_type", "url", "file_id", "data"]))
            .transpose()?
            .flatten();
        let media_type = src.as_ref().map(|r| r.string("media_type")).transpose()?.flatten();
        let marker = ty == "image";
        let data_ref = if let Some(src) = src {
            match src.string("type")?.as_deref() {
                Some("url") => src.string("url")?,
                Some("file") => src.string("file_id")?.map(|id| format!("file:{id}")),
                Some("base64") | None if marker => {
                    if src.get("data").map(|v| v.is_string()).transpose()?.unwrap_or(false) {
                        Some("base64:inline".into())
                    } else {
                        None
                    }
                }
                Some(other) if marker => Some(other.to_owned()),
                _ => None,
            }
        } else {
            None
        };
        return Ok(Some(if marker {
            Block::Image { media_type, data_ref }
        } else {
            Block::File {
                mime: media_type,
                path: record.string("title")?,
                source: data_ref,
            }
        }));
    }
    let keys: &[&str] = if ty == "text" {
        &["type"]
    } else if ty == "thinking" {
        &["type", q.thinking.1]
    } else if Some(ty.as_str()) == q.redacted_thinking {
        &["type", "data"]
    } else if ty == "tool_result" {
        &["type", "tool_use_id", "is_error"]
    } else {
        return Ok(None);
    };
    let mut fields = record.project(keys)?;
    let mut result = record.get("content").map(&mut *text).transpose()?.flatten();
    if ty == "tool_result" && result.is_none() {
        if let Some(content) = record.get("content") {
            if content.is_array()? && content.source_range()?.is_some_and(|(_, len)| len > joined_over) {
                result = Some(coerce_selected_array(content)?);
            } else {
                fields.insert("content".into(), content.resolve()?.into_owned());
            }
        }
    }
    let body = record.get("text").map(&mut *text).transpose()?.flatten();
    let thinking = record.get(q.thinking.0).map(&mut *text).transpose()?.flatten();
    Ok(block_from_fields(&Value::Object(fields), q, &mut |key| {
        if key == "content" {
            result.clone()
        } else if key == q.thinking.0 {
            thinking.clone()
        } else if key == "text" {
            body.clone()
        } else {
            None
        }
    }))
}

/// The exact coerce_text array policy: images become a marker, all string text parts (even
/// empty ones) retain their separator positions. Original capture guards follow joined backing.
fn coerce_selected_array(content: crate::json::JsonView<'_>) -> anyhow::Result<Text> {
    use crate::harness::store::{document::Fields, text};
    let mut parts = content.array()?.unwrap().filter_map(|item| {
        let selected = (|| -> anyhow::Result<Option<Option<crate::json::JsonView<'_>>>> {
            let Some(item) = Fields::parse(item?, &["type", "text"])? else {
                return Ok(None);
            };
            if item.string("type")?.as_deref().is_some_and(super::is_image_part) {
                return Ok(Some(None));
            }
            if let Some(value) = item.get("text") {
                if value.is_string()? {
                    return Ok(Some(Some(value)));
                }
            }
            Ok(None)
        })();
        match selected {
            Ok(Some(part)) => Some(Ok(part)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    });
    let Some(first) = parts.next().transpose()? else {
        return Ok(Text::default());
    };
    let Some(second) = parts.next().transpose()? else {
        return match first {
            None => Ok("[image]".into()),
            Some(value) => Ok(value.source_text()?.unwrap()),
        };
    };
    let crate::Json::Source(range) = content.to_json()? else {
        unreachable!("selected array has source backing")
    };
    let source = range.captured_source();
    text::append(source, &source.decoded_backing()?, |builder| {
        for (i, part) in [Ok(first), Ok(second)].into_iter().chain(parts).enumerate() {
            if i != 0 {
                builder.push("\n")?;
            }
            if let Some(part) = part? {
                anyhow::ensure!(builder.push_string(part)?, "selected tool-result text");
            } else {
                builder.push("[image]")?;
            }
        }
        Ok(())
    })
}

/// Every block of a `content` array; a bare string is one text block. (Claude's lazy reader spans
/// the bare-string case itself, so it walks the array with [`block_from_with`] instead.)
pub fn blocks_from(content: Option<&Value>, q: &Quirks) -> Vec<Block> {
    match content {
        Some(Value::String(s)) => vec![Block::Text { text: s.clone().into() }],
        Some(Value::Array(items)) => items.iter().filter_map(|i| block_from(i, q)).collect(),
        _ => Vec::new(),
    }
}

// ------------------------------------------------------------------------------------------------
// writing
// ------------------------------------------------------------------------------------------------

/// An assistant turn's blocks as content: text, thinking (redacted as the store's redacted block,
/// or a plain one when it has none), tool invocations, images; a [`Block::File`] becomes a
/// `[file: …]` text marker (documents are a user-turn shape); tool results are skipped (they go
/// on the Tool turn via [`tool_result_blocks`]).
pub fn assistant_blocks(content: &[Block], q: &Quirks) -> anyhow::Result<Vec<Value>> {
    blocks_to(content, q, false)
}

/// A user turn's blocks as content: text and images natively, a [`Block::File`] as the
/// `document` attachment the reader turns back into a File (stores with flat media get the text
/// marker), a stray tool result (mixed user content) as a `tool_result` block; thinking and tool
/// invocations do not occur on user turns but map to their native shapes rather than vanishing.
pub fn user_blocks(content: &[Block], q: &Quirks) -> anyhow::Result<Vec<Value>> {
    blocks_to(content, q, true)
}

/// A Tool turn's results as `tool_result` blocks the reader pairs by id.
pub fn tool_result_blocks(content: &[Block]) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    for block in content.iter().filter(|b| matches!(b, Block::ToolResult { .. })) {
        if let Some(value) = block_to(block, &ANTHROPIC, true)? {
            out.push(value);
        }
    }
    Ok(out)
}

fn blocks_to(content: &[Block], q: &Quirks, user: bool) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    for block in content {
        if let Some(value) = block_to(block, q, user)? {
            out.push(value);
        }
    }
    Ok(out)
}

/// Owned-codec output keeps its historical Span schema, but Source text errors are fallible
/// and its already-owned decoded string can move directly into the output Value.
fn text_value(text: &Text) -> anyhow::Result<Value> {
    match text {
        Text::Inline(value) => Ok(Value::String(value.clone())),
        Text::Span(span) => serde_json::to_value(span).map_err(Into::into),
        Text::Source(source) => Ok(Value::String(source.try_resolve()?)),
    }
}

fn block_to(b: &Block, q: &Quirks, user: bool) -> anyhow::Result<Option<Value>> {
    Ok(Some(match b {
        Block::Text { text } => {
            let mut fields = Map::new();
            fields.insert("type".into(), json!("text"));
            fields.insert("text".into(), text_value(text)?);
            Value::Object(fields)
        }
        Block::Thinking {
            text,
            signature,
            encrypted,
            redacted,
        } => {
            let mut m = Map::new();
            match q.redacted_thinking {
                Some(rt) if *redacted => {
                    m.insert("type".into(), json!(rt));
                    if let Some(enc) = encrypted {
                        m.insert("data".into(), json!(enc));
                    }
                }
                _ => {
                    m.insert("type".into(), json!("thinking"));
                    m.insert(q.thinking.0.into(), text_value(text)?);
                    if let Some(sig) = signature {
                        m.insert(q.thinking.1.into(), json!(sig));
                    }
                }
            }
            Value::Object(m)
        }
        Block::ToolUse { id, name, input, .. } => {
            let mut fields = Map::new();
            fields.insert("type".into(), json!(q.tool_use.0));
            fields.insert("id".into(), json!(id));
            fields.insert("name".into(), json!(name));
            fields.insert(q.tool_use.1.into(), input.resolve()?.into_owned());
            Value::Object(fields)
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } if user => {
            let mut fields = Map::new();
            fields.insert("type".into(), json!("tool_result"));
            fields.insert("tool_use_id".into(), json!(tool_use_id));
            fields.insert("content".into(), text_value(content)?);
            fields.insert("is_error".into(), json!(is_error));
            Value::Object(fields)
        }
        Block::ToolResult { .. } => return Ok(None),
        Block::Image { media_type, data_ref } => image_block(media_type, data_ref, q),
        Block::File { mime, path, source } if user && q.flat_media.is_none() => {
            // Inverse of `document_from`: title ← path, source ← url/file_id.
            let mut src = Map::new();
            if let Some(mt) = mime {
                src.insert("media_type".into(), json!(mt));
            }
            match source.as_deref() {
                Some(r) if r.starts_with("file:") => {
                    src.insert("type".into(), json!("file"));
                    src.insert("file_id".into(), json!(&r["file:".len()..]));
                }
                Some(r) => {
                    src.insert("type".into(), json!("url"));
                    src.insert("url".into(), json!(r));
                }
                None => {}
            }
            let mut doc = Map::new();
            doc.insert("type".into(), json!("document"));
            if let Some(p) = path {
                doc.insert("title".into(), json!(p));
            }
            doc.insert("source".into(), Value::Object(src));
            Value::Object(doc)
        }
        Block::File { path, source, .. } => {
            let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
            json!({ "type": "text", "text": format!("[file: {label}]") })
        }
    }))
}

/// An `image` block rebuilt from the IR's reference so the pointer survives a round trip (the
/// reader keeps a ref, not the bytes): the three `source` shapes [`image_from`] reads, or the
/// flat `mimeType`/`data` pair.
pub fn image_block(media_type: &Option<String>, data_ref: &Option<String>, q: &Quirks) -> Value {
    if q.flat_media.is_some() {
        let mut m = Map::new();
        m.insert("type".into(), json!("image"));
        if let Some(mt) = media_type {
            m.insert("mimeType".into(), json!(mt));
        }
        if let Some(d) = data_ref {
            m.insert("data".into(), json!(d));
        }
        return Value::Object(m);
    }
    let mut source = Map::new();
    if let Some(mt) = media_type {
        source.insert("media_type".into(), json!(mt));
    }
    match data_ref.as_deref() {
        Some(r) if r.starts_with("file:") => {
            source.insert("type".into(), json!("file"));
            source.insert("file_id".into(), json!(&r["file:".len()..]));
        }
        Some("base64:inline") => {
            // Bytes were intentionally not retained in the IR; mark the kind.
            source.insert("type".into(), json!("base64"));
        }
        Some(r) => {
            source.insert("type".into(), json!("url"));
            source.insert("url".into(), json!(r));
        }
        None => {}
    }
    json!({ "type": "image", "source": Value::Object(source) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_anthropic_block() {
        let items = json!([
            {"type": "text", "text": ""},
            {"type": "thinking", "thinking": "hm", "signature": ""},
            {"type": "redacted_thinking", "data": "BLOB"},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "x"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "ok"}, {"type": "image"}], "is_error": true},
            {"type": "image", "source": {"type": "file", "file_id": "f9", "media_type": "image/png"}},
            {"type": "document", "title": "a.pdf", "source": {"type": "url", "url": "https://x/a.pdf", "media_type": "application/pdf"}},
            {"type": "server_tool_use"}
        ]);
        let b = blocks_from(Some(&items), &ANTHROPIC);
        assert_eq!(b.len(), 7, "unknown types are skipped, empty text is kept");
        assert!(matches!(&b[0], Block::Text { text } if text.is_empty()));
        assert!(matches!(&b[1], Block::Thinking { signature: Some(s), redacted: false, .. } if s.is_empty()));
        assert!(matches!(&b[2], Block::Thinking { encrypted: Some(e), redacted: true, .. } if e == "BLOB"));
        assert!(matches!(&b[3], Block::ToolUse { name, .. } if name == "Read"));
        assert!(matches!(&b[4], Block::ToolResult { content, is_error: true, .. } if content == "ok\n[image]"));
        assert!(matches!(&b[5], Block::Image { data_ref: Some(r), .. } if r == "file:f9"));
        assert!(
            matches!(&b[6], Block::File { path: Some(p), source: Some(s), .. } if p == "a.pdf" && s == "https://x/a.pdf")
        );
    }

    #[test]
    fn quirks_rename_keys() {
        let oc = json!([
            {"type": "toolCall", "id": "c", "name": "exec", "arguments": {"cmd": "ls"}},
            {"type": "thinking", "thinking": "t", "thinkingSignature": "S"},
            {"type": "image", "mimeType": "image/png", "data": "0123456789"}
        ]);
        let b = blocks_from(Some(&oc), &OPENCLAW);
        assert!(matches!(&b[0], Block::ToolUse { input, .. } if input.resolve().unwrap()["cmd"] == "ls"));
        assert!(matches!(&b[1], Block::Thinking { signature: Some(s), .. } if s == "S"));
        assert!(
            matches!(&b[2], Block::Image { media_type: Some(m), data_ref: Some(d) } if m == "image/png" && d == "0123456789")
        );
        let zed = json!([{"type": "text", "text": ""}, {"type": "thinking", "text": "z", "signature": ""}, {"type": "RedactedThinking", "data": "D"}]);
        let b = blocks_from(Some(&zed), &ZED);
        assert_eq!(b.len(), 2, "Zed drops empty text");
        assert!(matches!(&b[0], Block::Thinking { text, signature: None, .. } if text == "z"));
        assert!(matches!(&b[1], Block::Thinking { redacted: true, .. }));
        let goose = json!([{"type": "redactedThinking", "data": "D"}, {"type": "document", "name": "n.pdf", "mimeType": "application/pdf", "data": "AAAA"}]);
        let b = blocks_from(Some(&goose), &GOOSE);
        assert!(matches!(&b[0], Block::Thinking { redacted: true, .. }));
        assert!(
            matches!(&b[1], Block::File { path: Some(p), source: Some(s), .. } if p == "n.pdf" && s == "base64:inline")
        );
    }

    #[test]
    fn writes_what_it_reads() {
        let blocks = vec![
            Block::Text { text: "hi".into() },
            Block::Thinking {
                text: "".into(),
                signature: None,
                encrypted: Some("BLOB".into()),
                redacted: true,
            },
            Block::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: json!({"path": "x"}).into(),
                namespace: None,
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("base64:inline".into()),
            },
            Block::File {
                mime: None,
                path: Some("a.pdf".into()),
                source: Some("file:f9".into()),
            },
            Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
        ];
        let a = assistant_blocks(&blocks, &ANTHROPIC).unwrap();
        assert_eq!(a.len(), 5, "tool results are not assistant content");
        assert_eq!(a[1]["type"], "redacted_thinking");
        assert_eq!(a[4]["type"], "text", "a file on an assistant turn is a marker");
        let u = user_blocks(&blocks, &ANTHROPIC).unwrap();
        assert_eq!(u[4]["type"], "document");
        assert_eq!(u[5]["type"], "tool_result");
        let back = blocks_from(Some(&Value::Array(u)), &ANTHROPIC);
        assert_eq!(back.len(), blocks.len());
        assert!(
            matches!(&back[4], Block::File { path: Some(p), source: Some(s), .. } if p == "a.pdf" && s == "file:f9")
        );
        // OpenClaw has no redacted block: the thinking block is written plain, under its keys.
        let oc = assistant_blocks(&blocks, &OPENCLAW).unwrap();
        assert_eq!(oc[1]["type"], "thinking");
        assert_eq!(oc[2]["type"], "toolCall");
        assert_eq!(oc[2]["arguments"]["path"], "x");
        assert_eq!(oc[3]["mimeType"], "image/png");
        assert_eq!(tool_result_blocks(&blocks).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod selected_media_tests {
    use super::*;
    #[test]
    fn owned_codec_returns_changed_text_errors_instead_of_panicking() -> anyhow::Result<()> {
        let world = crate::test_world("anthropic-owned-source-error");
        let path = world.base.join("text.json");
        let raw = serde_json::to_vec(&"large text ".repeat(10000))?;
        std::fs::write(&path, &raw)?;
        let text = crate::json::JsonSource::capture(&path)?
            .read_range_value(0, raw.len() as u64, |view| view.source_text())?
            .unwrap();
        std::fs::write(&path, b"changed source")?;
        for block in [
            Block::Text { text: text.clone() },
            Block::Thinking {
                text: text.clone(),
                signature: None,
                encrypted: None,
                redacted: false,
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: text,
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
        ] {
            assert!(block_to(&block, &ANTHROPIC, true).is_err());
        }
        Ok(())
    }
    #[test]
    fn wide_composite_results_preserve_empty_parts_image_precedence_and_guard() -> anyhow::Result<()> {
        let world = crate::test_world("wire-wide-result");
        let path = world.base.join("result.json");
        let mut parts = Vec::new();
        for i in 0..8192 {
            parts.push(match i % 4 {
                0 => json!({"text":""}),
                1 => json!({"ignored":"not a text part"}),
                2 => json!({"type":"image_url","text":"ignored image text"}),
                _ => json!({"text":format!("part-{i} λ\n")}),
            });
        }
        let value = json!({"type":"tool_result","tool_use_id":"call","content":parts});
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let block = source
            .read_range_value(0, source.len(), |view| {
                block_from_view_with(view, &ANTHROPIC, true, &mut |v| v.text(true), 0)
            })?
            .unwrap();
        assert_eq!(
            serde_json::to_value(&block)?,
            serde_json::to_value(block_from(&value, &ANTHROPIC))?
        );
        std::fs::write(&path, b"changed")?;
        if let Block::ToolResult { content, .. } = block {
            assert!(content.try_resolve(&crate::lazy::Resolver::new(None)).is_err());
        } else {
            panic!("tool result");
        }
        Ok(())
    }

    #[test]
    fn composite_result_projection_matches_owned_codec_without_joined_body_ownership() -> anyhow::Result<()> {
        let world = crate::test_world("wire-composite-result");
        let path = world.base.join("result.json");
        let body = "λ\n\"result\"".repeat(20000);
        let value = json!({"type":"tool_result","tool_use_id":"call","is_error":true,"content":[{"text":""},{"text":body,"ignored":body},{"type":"image_url","text":"ignored image text"},{"text":""},{"text":body}],"ignored":body});
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let block = source
            .read_range_value(0, source.len(), |view| {
                block_from_view_with(view, &ANTHROPIC, true, &mut |v| v.text(true), 0)
            })?
            .unwrap();
        assert!(matches!(
            &block,
            Block::ToolResult {
                content: Text::Source(_),
                ..
            }
        ));
        let mut session = crate::Session::new("result", crate::Harness::Claude);
        let mut message = crate::Message::new(crate::Role::Tool);
        message.content.push(block);
        session.messages.push(message);
        session.materialize()?;
        assert_eq!(
            serde_json::to_value(&session.messages[0].content[0])?,
            serde_json::to_value(block_from(&value, &ANTHROPIC))?
        );
        let block = source
            .read_range_value(0, source.len(), |view| {
                block_from_view_with(view, &ANTHROPIC, true, &mut |v| v.text(true), 0)
            })?
            .unwrap();
        std::fs::write(&path, b"changed after selected join")?;
        if let Block::ToolResult { content, .. } = block {
            assert!(content.try_resolve(&crate::lazy::Resolver::new(None)).is_err());
        }
        Ok(())
    }
    #[test]
    fn flat_media_projection_matches_owned_codec_with_large_ignored_payloads() -> anyhow::Result<()> {
        let world = crate::test_world("wire-flat-media");
        let path = world.base.join("part.json");
        for quirks in [OPENCLAW, GOOSE] {
            for ty in ["image", "document"] {
                for data in [json!(""), json!("λ\n\"".repeat(20000)), json!(false)] {
                    let value = json!({"type":ty,"mimeType":"image/png","data":data,"name":"attachment","unknown":"x".repeat(200000)});
                    std::fs::write(&path, serde_json::to_vec(&value)?)?;
                    let source = crate::json::JsonSource::capture(&path)?;
                    let selected = source.read_range_value(0, source.len(), |view| {
                        block_from_view_with(view, &quirks, true, &mut |v| v.text(true), 0)
                    })?;
                    assert_eq!(
                        serde_json::to_value(selected)?,
                        serde_json::to_value(block_from(&value, &quirks))?
                    );
                }
            }
        }
        Ok(())
    }
}
