//! Rendering an IR [`Session`] to human/LLM-friendly text. Shared by the `cv` CLI and `cv mcp`.

use crate::ir::*;
use chrono::{DateTime, Utc};
use std::io::Write;
use std::path::Path;

/// Write complete Markdown with bounded scratch for lazy text and JSON leaves. Callers must
/// discard staged output on error: a source or writer failure may follow an emitted prefix.
pub fn write_markdown(s: &Session, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    let reads = crate::source::session_reads(s);
    let label = match &s.title {
        Some(title) => truncate(title, 72),
        None => {
            let mut label = None;
            for message in s.messages.iter().filter(|m| m.role == Role::User) {
                if let Some(candidate) = message_label(message, resolver)? {
                    label = Some(candidate);
                    break;
                }
            }
            label.unwrap_or_else(|| "(untitled)".into())
        }
    };
    out.write_all(
        md_header(
            &label,
            s.harness,
            &s.id,
            s.cwd.as_deref(),
            s.model.as_deref(),
            s.created_at,
        )
        .as_bytes(),
    )?;
    for message in &s.messages {
        write_md_message(message, out, resolver)?;
    }
    anyhow::ensure!(
        reads.fresh(&mut crate::source::Probe::default()),
        "session sources changed during Markdown output"
    );
    Ok(())
}

/// The exact label projection of a message's concatenated plain text, retaining at most 73
/// scalars. Complete checked demands preserve whitespace-only rejection and late source errors.
pub fn message_label(m: &Message, resolver: &crate::Resolver) -> anyhow::Result<Option<String>> {
    plain_text_preview(m, resolver, 72, true)
}

/// Exact character-bounded preview of concatenated plain-text blocks, including whitespace-only
/// text. Checked demands scan discarded tails without retaining them.
pub fn message_text_preview(m: &Message, resolver: &crate::Resolver, chars: usize) -> anyhow::Result<Option<String>> {
    plain_text_preview(m, resolver, chars, false)
}

fn plain_text_preview(
    m: &Message,
    resolver: &crate::Resolver,
    chars: usize,
    require_meaningful: bool,
) -> anyhow::Result<Option<String>> {
    let mut head = String::new();
    let mut remaining = chars.saturating_add(1);
    let mut nonempty = false;
    let mut meaningful = false;
    let mut append = |part: &str| {
        if !meaningful {
            meaningful = part.chars().any(|c| !c.is_whitespace());
        }
        for c in part.chars().take(remaining) {
            head.push(c);
            remaining -= 1;
        }
    };
    for block in &m.content {
        if let Block::Text { text } = block {
            if nonempty {
                append("\n");
            }
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                nonempty |= !part.is_empty();
                append(part);
                Ok(())
            })?;
        }
    }
    Ok((if require_meaningful { meaningful } else { nonempty }).then(|| truncate(&head, chars)))
}

/// Write one Markdown section, preserving the owned renderer's exact formatting.
pub fn write_md_message(m: &Message, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    write!(out, "## {}\n\n", turn_label(m))?;
    for block in &m.content {
        write_md_block(block, out, resolver)?;
    }
    Ok(())
}

/// Write a Markdown block. Result excerpts retain only 4001 decoded scalars, while the entire
/// source is demanded so discarded tails cannot hide read errors or stale dependencies.
pub fn write_md_block(b: &Block, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    match b {
        Block::Text { text } => {
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                out.write_all(part.as_bytes())?;
                Ok(())
            })?;
            out.write_all(b"\n\n")?;
        }
        Block::Thinking { text, .. } => {
            out.write_all("> 🧠 ".as_bytes())?;
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                for (i, line) in part.split('\n').enumerate() {
                    if i != 0 {
                        out.write_all(b"\n> ")?;
                    }
                    out.write_all(line.as_bytes())?;
                }
                Ok(())
            })?;
            out.write_all(b"\n\n")?;
        }
        Block::ToolUse { name, input, .. } => {
            write!(out, "**🔧 {name}**\n\n```json\n")?;
            input.write_canonical(out)?;
            out.write_all(b"\n```\n\n")?;
        }
        Block::ToolResult { content, is_error, .. } => {
            let mut head = String::new();
            let mut remaining = 4001;
            content.try_for_each_chunk(resolver, 64 * 1024, |part| {
                for c in part.chars().take(remaining) {
                    head.push(c);
                    remaining -= 1;
                }
                Ok(())
            })?;
            // Tool output is a literal code excerpt, so preserve its line structure.
            // The extra scalar distinguishes an exact fit from a truncated result.
            if remaining == 0 {
                head.pop();
                head.pop();
                head.push('…');
            }
            let excerpt = head;
            // A tool can return Markdown containing its own fenced blocks. Keep that text
            // literal by choosing a fence longer than every run in the rendered excerpt.
            let longest = excerpt.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(3.max(longest + 1));
            write!(
                out,
                "**↩ result{}**\n\n{fence}\n{}\n{fence}\n\n",
                if *is_error { " (error)" } else { "" },
                excerpt
            )?;
        }
        Block::File { path, source, .. } => write!(out, "_[file: {}]_\n\n", file_label(path, source))?,
        Block::Image { .. } => out.write_all(b"_[image]_\n\n")?,
    }
    Ok(())
}

/// Render a session as Markdown (good for sharing / feeding to another model). The streaming
/// `cv export --format md` renders the same header and sections one message at a time through
/// [`md_header`] and [`md_message`].
pub fn to_markdown(s: &Session) -> anyhow::Result<String> {
    let mut out = Vec::new();
    write_markdown(s, &mut out, &s.resolver())?;
    Ok(String::from_utf8(out)?)
}

/// The Markdown front-matter: title, harness, id, cwd (home-relative), model and start time, each
/// only when known, then a blank line.
pub fn md_header(
    label: &str,
    harness: Harness,
    id: &str,
    cwd: Option<&Path>,
    model: Option<&str>,
    started: Option<DateTime<Utc>>,
) -> String {
    let mut out = format!("# {label}\n\n- harness: {harness}\n- id: {id}\n");
    if let Some(cwd) = cwd {
        out.push_str(&format!("- cwd: {}\n", home_rel(cwd)));
    }
    if let Some(m) = model {
        out.push_str(&format!("- model: {m}\n"));
    }
    if let Some(t) = started {
        out.push_str(&format!("- started: {}\n", t.to_rfc3339()));
    }
    out.push('\n');
    out
}

/// One Markdown section per turn: `## <turn label>` then every block through [`md_block`].
pub fn md_message(m: &Message, out: &mut String) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    write_md_message(m, &mut bytes, &crate::Resolver::new(None))?;
    out.push_str(&String::from_utf8(bytes)?);
    Ok(())
}

/// Append one block using the checked Markdown writer. Use `write_md_block` to avoid retaining
/// the complete rendered block when writing a stream or staging a document.
pub fn md_block(b: &Block, out: &mut String) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    write_md_block(b, &mut bytes, &crate::Resolver::new(None))?;
    out.push_str(&String::from_utf8(bytes)?);
    Ok(())
}

fn file_label(path: &Option<String>, source: &Option<String>) -> String {
    path.as_deref().or(source.as_deref()).unwrap_or("?").to_string()
}

pub fn role_label(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// The label a rendered turn gets: the role, refined by its `kind` whenever the kind says more than
/// the role does (`system · compaction_boundary`, `user · compaction_summary`, `system · error`),
/// and for injected context by the harness's own attachment kind when it recorded one
/// (`system · injected_context/hook_success`). A plain prompt or reply is just `user` / `assistant`.
pub fn turn_label(m: &Message) -> String {
    let role = role_label(m.role);
    if m.kind == MessageKind::for_role(m.role) {
        return role.to_string();
    }
    let attachment_kind = m
        .extra
        .values()
        .filter_map(serde_json::Value::as_object)
        .find_map(|bag| bag.get("attachment_type"))
        .and_then(serde_json::Value::as_str);
    match (m.kind, attachment_kind) {
        (MessageKind::InjectedContext, Some(k)) => format!("{role} · {}/{k}", m.kind.as_str()),
        _ => format!("{role} · {}", m.kind.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Source(std::path::PathBuf);
    impl Source {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("cv-render-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
        fn json(&self) -> crate::Json {
            crate::Json::source(&self.0, 0, std::fs::metadata(&self.0).unwrap().len()).unwrap()
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn source_text(value: &crate::Json, key: &str) -> crate::Text {
        value
            .select(key)
            .unwrap()
            .unwrap()
            .with_view(|view| Ok(view.source_text()?.unwrap()))
            .unwrap()
    }
    fn old_md_block(b: &Block, out: &mut String) -> anyhow::Result<()> {
        match b {
            Block::Text { text } => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            Block::Thinking { text, .. } => {
                out.push_str("> 🧠 ");
                out.push_str(&text.replace('\n', "\n> "));
                out.push_str("\n\n");
            }
            Block::ToolUse { name, input, .. } => {
                out.push_str(&format!("**🔧 {name}**\n\n```json\n"));
                crate::projection::append_json(out, input)?;
                out.push_str("\n```\n\n");
            }
            Block::ToolResult { content, is_error, .. } => {
                out.push_str(&format!(
                    "**↩ result{}**\n\n```\n{}\n```\n\n",
                    if *is_error { " (error)" } else { "" },
                    {
                        let mut excerpt: String = content.chars().take(4000).collect();
                        if content.chars().count() > 4000 {
                            excerpt.pop();
                            excerpt.push('…');
                        }
                        excerpt
                    }
                ));
            }
            Block::File { path, source, .. } => {
                out.push_str(&format!("_[file: {}]_\n\n", file_label(path, source)));
            }
            Block::Image { .. } => out.push_str("_[image]_\n\n"),
        }
        Ok(())
    }

    fn old_markdown(session: &Session) -> String {
        let mut out = md_header(
            &session.label(),
            session.harness,
            &session.id,
            session.cwd.as_deref(),
            session.model.as_deref(),
            session.created_at,
        );
        for message in &session.messages {
            out.push_str(&format!("## {}\n\n", turn_label(message)));
            for block in &message.content {
                old_md_block(block, &mut out).unwrap();
            }
        }
        out
    }
    #[test]
    fn markdown_title_skips_empty_user_turns_and_propagates_candidate_errors() -> anyhow::Result<()> {
        let mut session = Session::new("title", Harness::Codex);
        for text in ["   ", "actual title"] {
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.into() });
            session.messages.push(message);
        }
        assert!(to_markdown(&session)?.starts_with("# actual title\n"));
        let world = crate::test_world("markdown-title-source");
        let path = world.base.join("title.json");
        std::fs::write(&path, serde_json::to_vec(&"lazy title".repeat(2000))?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let text = source
            .read_range_value(0, source.len(), |view| view.source_text())?
            .unwrap();
        session.messages[1].content = vec![Block::Text { text }];
        std::fs::write(&path, b"changed title source")?;
        assert!(to_markdown(&session).is_err());
        Ok(())
    }

    #[test]
    fn markdown_tool_results_keep_nested_fences_literal() {
        let mut output = Vec::new();
        let block = Block::ToolResult {
            tool_use_id: "tool".into(),
            content: "before\n```rust\nlet x = 1;\n```\nafter".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        };
        write_md_block(&block, &mut output, &crate::Resolver::new(None)).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "**↩ result**\n\n````\nbefore\n```rust\nlet x = 1;\n```\nafter\n````\n\n"
        );
    }

    #[test]
    fn markdown_result_excerpt_preserves_newlines_and_scalar_limit() {
        for length in [3999, 4000, 4001] {
            let body = format!("line\n{}", "α".repeat(length - 5));
            let block = Block::ToolResult {
                tool_use_id: "tool".into(),
                content: body.clone().into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            };
            let mut output = Vec::new();
            write_md_block(&block, &mut output, &crate::Resolver::new(None)).unwrap();
            let output = String::from_utf8(output).unwrap();
            let excerpt = output
                .strip_prefix("**↩ result**\n\n```\n")
                .unwrap()
                .strip_suffix("\n```\n\n")
                .unwrap();
            assert!(excerpt.starts_with("line\n"));
            assert_eq!(excerpt.chars().count(), length.min(4000));
            if length > 4000 {
                assert!(excerpt.ends_with('…'));
            } else {
                assert_eq!(excerpt, body);
            }
        }
    }

    #[test]
    fn checked_session_writers_match_complete_owned_format_oracles() {
        let fixture = serde_json::json!({
            "text": "α\n🌙\\\"".repeat(20000),
            "result": "🌙\n".repeat(3000),
            "input": {"nested": [[], {}, "α\n\\\"", 1.25]}
        });
        let source = Source::new(&serde_json::to_vec(&fixture).unwrap());
        let value = source.json();
        let mut session = Session::new("checked", Harness::Codex);
        session.title = Some("complete formatting".into());
        session.model = Some("model".into());
        session.cwd = Some("/tmp/fixture".into());
        session.system_prompt = Some("system\n🌙".into());
        session
            .extra
            .insert("cv".into(), serde_json::json!({"loom": [{"source_id": "one"}]}));
        let mut message = Message::new(Role::User);
        message.content = vec![
            Block::Text {
                text: source_text(&value, "text"),
            },
            Block::Thinking {
                text: source_text(&value, "text"),
                signature: None,
                encrypted: None,
                redacted: false,
            },
            Block::ToolUse {
                id: "call".into(),
                name: "tool".into(),
                namespace: None,
                input: value.select("input").unwrap().unwrap(),
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: source_text(&value, "result"),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
            },
            Block::File {
                mime: None,
                path: None,
                source: Some("asset".into()),
            },
            Block::Image {
                media_type: None,
                data_ref: None,
            },
        ];
        session.messages.push(message);
        session.messages.push(Message::new(Role::Assistant));
        let mut owned = session.clone();
        owned.materialize().unwrap();
        let resolver = session.resolver();
        let mut markdown = Vec::new();
        write_markdown(&session, &mut markdown, &resolver).unwrap();
        assert_eq!(markdown, old_markdown(&owned).as_bytes());
        assert_eq!(to_markdown(&session).unwrap(), old_markdown(&owned));
        session.title = None;
        owned.title = None;
        markdown.clear();
        write_markdown(&session, &mut markdown, &resolver).unwrap();
        assert_eq!(markdown, old_markdown(&owned).as_bytes());
        let mut compact = Vec::new();
        session.write_json(&mut compact, &resolver).unwrap();
        assert_eq!(compact, serde_json::to_vec(&owned).unwrap());
        let mut pretty = Vec::new();
        session.write_json_pretty(&mut pretty, &resolver).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&owned).unwrap());
        let empty = Session::new("empty", Harness::Claude);
        pretty.clear();
        empty.write_json_pretty(&mut pretty, &empty.resolver()).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&empty).unwrap());
    }

    #[test]
    fn checked_message_labels_match_join_trim_and_truncate_oracle() {
        for pieces in [
            vec!["", ""],
            vec!["\u{2003}\n", "\t"],
            vec!["", "alpha", ""],
            vec![" \n", "🌙", "tail"],
        ] {
            let mut m = Message::new(Role::User);
            for text in pieces {
                m.content.push(Block::Text { text: text.into() });
            }
            assert_eq!(
                message_label(&m, &crate::Resolver::new(None)).unwrap(),
                m.text().filter(|t| !t.trim().is_empty()).map(|t| truncate(&t, 72))
            );
            assert_eq!(
                message_text_preview(&m, &crate::Resolver::new(None), 80).unwrap(),
                m.text().map(|t| truncate(&t, 80))
            );
        }
        let text = format!("{}🌙", " ".repeat(100000));
        let source = Source::new(&serde_json::to_vec(&serde_json::json!({"text":text})).unwrap());
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: source_text(&source.json(), "text"),
        });
        assert_eq!(
            message_label(&m, &crate::Resolver::new(None)).unwrap(),
            Some(truncate(&text, 72))
        );
    }

    #[test]
    fn staged_session_outputs_reject_a_source_changed_after_its_last_leaf() {
        struct Mutating<'a> {
            stage: crate::bytes::ByteStage,
            path: &'a Path,
            changed: bool,
        }
        impl Write for Mutating<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                // The later message's role is emitted after all first-message lazy demands.
                if !self.changed && bytes.windows(9).any(|part| part == b"assistant") {
                    std::fs::remove_file(self.path)?;
                    self.changed = true;
                }
                self.stage.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.stage.flush()
            }
        }
        for json in [false, true] {
            let source = Source::new(&serde_json::to_vec(&serde_json::json!({"text":"early".repeat(1000)})).unwrap());
            let mut session = Session::new("guarded", Harness::Codex);
            let mut first = Message::new(Role::User);
            first.content.push(Block::Text {
                text: source_text(&source.json(), "text"),
            });
            assert!(matches!(&first.content[0], Block::Text { text } if text.is_span()));
            session.messages = vec![first, Message::new(Role::Assistant)];
            let mut writer = Mutating {
                stage: crate::bytes::ByteStage::new(),
                path: &source.0,
                changed: false,
            };
            let result = if json {
                session.write_json_pretty(&mut writer, &session.resolver())
            } else {
                write_markdown(&session, &mut writer, &session.resolver())
            };
            assert!(writer.changed);
            assert!(result.is_err());
            assert!(!writer.stage.is_empty());
            // No finished ByteSource exists for publication after an error.
        }
    }
}
