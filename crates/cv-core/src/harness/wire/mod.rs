//! The two wire shapes most harnesses persist, read into [`Block`](crate::ir::Block)s and written
//! back out, each spelled **once**:
//!
//! - [`anthropic`]: the Messages-API content array (`text | thinking | redacted_thinking | tool_use
//!   | tool_result | image | document`) — Claude Code, Cline/Roo, the claude.ai export, and with a
//!   renamed key or two OpenClaw, Goose and Zed's legacy threads.
//! - [`openai`]: chat/Responses parts (`text | input_text | output_text | image_url | input_image`)
//!   plus `tool_calls[].function.arguments` as a JSON **string** — Codex, Kimi, Kimi Code, Hermes,
//!   Continue, Grok.
//!
//! An adapter names its store's spelling with a `Quirks` constant and keeps only what is genuinely
//! its own: where its files are and which records mean prompt, reply, tool, compaction.

use serde_json::Value;

pub mod anthropic;
pub mod openai;
pub(crate) mod write;

/// A string, or the string `text` fields of its parts joined by newlines. Keep the caller's
/// text rule for a singleton; multiple parts use the source's checked backing when supplied.
/// Empty text parts still contribute separators, and non-text parts contribute nothing.
pub(crate) fn joined_text(
    content: Option<crate::json::JsonView<'_>>,
    source: Option<&crate::json::JsonSource>,
    text: openai::TextRule<'_>,
) -> anyhow::Result<crate::Text> {
    let Some(content) = content else {
        return Ok(crate::Text::default());
    };
    if let Some(text) = text(content)? {
        return Ok(text);
    }
    let Some(parts) = content.array()? else {
        return Ok(crate::Text::default());
    };
    let mut strings = parts.filter_map(|part| {
        part.and_then(|part| Ok(part.get("text")?.filter(|v| v.is_string().unwrap_or(false))))
            .transpose()
    });
    let Some(first) = strings.next().transpose()? else {
        return Ok(crate::Text::default());
    };
    let Some(second) = strings.next().transpose()? else {
        return Ok(text(first)?.unwrap_or_default());
    };
    let strings = [Ok(first), Ok(second)].into_iter().chain(strings);
    match source {
        Some(source) => super::store::text::append(source, &source.decoded_backing()?, |out| {
            for (index, view) in strings.enumerate() {
                if index > 0 {
                    out.push("\n")?;
                }
                out.push_string(view?)?;
            }
            Ok(())
        }),
        None => {
            let mut joined = String::new();
            for (index, view) in strings.enumerate() {
                if index > 0 {
                    joined.push('\n');
                }
                joined.push_str(view?.resolve()?.as_str().unwrap_or_default());
            }
            Ok(crate::Text::Inline(joined))
        }
    }
}

/// A part is an image whatever the family calls it.
pub(crate) fn is_image_part(ty: &str) -> bool {
    matches!(ty, "image" | "image_url" | "input_image" | "output_image" | "imageUrl")
}

/// Flatten a `content` field — a plain string or an array of parts — to transcript text: text
/// parts joined by newlines, an image part as the `[image]` placeholder so it is not silently
/// dropped from the searchable transcript. For a tool result's content and a message body.
pub(crate) fn coerce_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| {
                if i.get("type").and_then(Value::as_str).is_some_and(is_image_part) {
                    return Some("[image]".to_string());
                }
                i.get("text").and_then(Value::as_str).map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The text parts of a `content` field only (no placeholders): for titles and cwd sniffing, where
/// `[image]` would be noise.
#[cfg(test)]
pub(crate) fn text_parts(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Discovery's exact text_parts title, retaining only the demanded head. The caller's checked
/// record view validates the entire row and rechecks its source after this projection.
pub(crate) fn text_title(content: Option<crate::json::JsonView<'_>>) -> anyhow::Result<Option<String>> {
    let Some(content) = content else { return Ok(None) };
    let resolver = crate::lazy::Resolver::new(None);
    if let Some(text) = content.source_text()? {
        return Ok(super::store::document::text_summary(
            std::iter::once(text),
            &resolver,
            true,
        ));
    }
    let Some(parts) = content.array()? else { return Ok(None) };
    let mut failure = None;
    let texts = parts.filter_map(|part| {
        match part.and_then(|part| {
            part.get("text")?
                .map(|value| value.source_text())
                .transpose()
                .map(Option::flatten)
        }) {
            Ok(text) => text,
            Err(error) => {
                failure = Some(error);
                None
            }
        }
    });
    let title = super::store::document::text_summary(texts, &resolver, true);
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(title)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn joined_text_preserves_parts_singleton_spans_and_source_guards() -> anyhow::Result<()> {
        let world = crate::test_world("wire-joined-text");
        let path = world.base.join("parts.json");
        let long = "λ\n\"".repeat(20_000);
        for value in [
            Value::Null,
            json!(""),
            json!(long),
            json!([]),
            json!([null, {"text": 3}, {"type": "image"}]),
            json!([{"text": ""}, {"text": ""}, {"text": "end"}]),
            json!([{"text": long}]),
            json!([{"text": long}, {"text": ""}, {"text": long}]),
        ] {
            let expected = text_parts(Some(&value));
            let owned = joined_text(Some((&value).into()), None, &mut |v| v.text(false))?;
            assert_eq!(owned, expected.as_str());
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            for primary_span in [false, true] {
                let source = crate::json::JsonSource::capture(&path)?;
                let text = source.read_range_value(0, source.len(), |view| {
                    joined_text(Some(view), Some(&source), &mut |v| {
                        if primary_span {
                            v.lazy_text(true)
                        } else {
                            v.source_text()
                        }
                    })
                })?;
                let resolver = crate::lazy::Resolver::new(Some(path.clone()));
                assert_eq!(text.try_resolve(&resolver)?.as_ref(), expected);
                if value.as_array().is_some_and(|parts| parts.len() == 1) && primary_span {
                    assert!(matches!(text, crate::Text::Span(_)));
                }
                if let crate::Text::Source(_) = &text {
                    std::fs::write(&path, b"null")?;
                    assert!(text.try_resolve(&resolver).is_err());
                    std::fs::write(&path, serde_json::to_vec(&value)?)?;
                }
            }
        }
        Ok(())
    }

    #[test]
    fn selected_title_matches_full_text_join_without_part_roster() -> anyhow::Result<()> {
        let world = crate::test_world("wire-title-parts");
        let path = world.base.join("content.json");
        let mut many = vec![serde_json::json!({"text":""}); 4096];
        many.push(serde_json::json!({"text":"late"}));
        for value in [
            serde_json::json!("large λ\n\"".repeat(20000)),
            serde_json::json!([{ "text":""},{"text":""},{"text":"answer"}]),
            serde_json::json!([{ "text":" \n"},{"ignored":"large"},{"text":"\u{2003}"}]),
            serde_json::json!(many),
            serde_json::json!({"text":"ignored non-array"}),
        ] {
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            let source = crate::json::JsonSource::capture(&path)?;
            let title = source.read_range_value(0, source.len(), |view| text_title(Some(view)))?;
            let full = text_parts(Some(&value));
            let expected = (!full.trim().is_empty()).then(|| crate::ir::truncate(&full, 80));
            assert_eq!(title, expected);
        }
        Ok(())
    }
    #[test]
    fn coerce_and_text_parts() {
        let arr = json!([
            {"type": "text", "text": "a"},
            {"type": "image", "source": {"type": "base64", "data": "…"}},
            {"type": "input_text", "text": "b"},
            {"type": "tool_use", "id": "x"}
        ]);
        assert_eq!(coerce_text(Some(&arr)), "a\n[image]\nb");
        assert_eq!(text_parts(Some(&arr)), "a\nb");
        assert_eq!(coerce_text(Some(&json!("plain"))), "plain");
        assert_eq!(text_parts(Some(&json!("plain"))), "plain");
        assert_eq!(coerce_text(None), "");
        assert_eq!(coerce_text(Some(&json!({"text": "not an array"}))), "");
    }
}
