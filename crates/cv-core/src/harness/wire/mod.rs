//! The wire shapes most harnesses persist, read into [`Block`](crate::ir::Block)s and written
//! back out, each spelled **once**:
//!
//! - [`anthropic`]: the Messages-API content array (`text | thinking | redacted_thinking | tool_use
//!   | tool_result | image | document`) — Claude Code, Cline/Roo, the claude.ai export, and with a
//!   renamed key or two OpenClaw, Goose and Zed's legacy threads.
//! - [`openai`]: chat/Responses parts (`text | input_text | output_text | image_url | input_image`)
//!   plus `tool_calls[].function.arguments` as a JSON **string** — Codex, Kimi, Kimi Code, Hermes,
//!   Continue, Grok.
//!
//! - [`genai`]: GenAI parts shared by Qwen and Gemini, retaining each store's tool wrappers.
//! - [`harmony`]: gpt-oss channel text decoded into blocks (LM Studio keeps it as plain text).
//!
//! An adapter names its store's spelling with a `Quirks` constant and keeps only what is genuinely
//! its own: where its files are and which records mean prompt, reply, tool, compaction.

#[cfg(test)]
use crate::json::{ReadInput, Validation};
use serde_json::Value;

pub mod anthropic;
pub(crate) mod genai;
pub mod harmony;
pub mod openai;
pub(crate) mod write;

/// Which parts [`joined`] keeps: the string under `text` of each part, and for a part whose
/// `type` is an image, `image` (a marker, so the image is not silently dropped) or nothing.
#[derive(Clone, Copy)]
pub(crate) struct Parts {
    pub text: &'static str,
    pub image: Option<&'static str>,
}
/// Text parts only: titles, message bodies.
pub(crate) const TEXT: Parts = Parts {
    text: "text",
    image: None,
};
/// A tool result's content: text parts, an image part as `[image]`.
pub(crate) const RESULT: Parts = Parts {
    text: "text",
    image: Some("[image]"),
};

/// Joined structured text stays in its source when spans are requested, otherwise inline.
pub(crate) const fn joined_over(spans: bool) -> u64 {
    if spans {
        0
    } else {
        u64::MAX
    }
}

/// Where several joined parts live.
#[derive(Clone, Copy)]
pub(crate) enum Backing<'a> {
    /// An owned string.
    Inline,
    /// The source's checked decoded backing, always.
    Source(&'a crate::json::JsonSource),
    /// The containing value's own source's backing when its range is over this many bytes,
    /// else inline (`u64::MAX`: always inline).
    Over(u64),
}

/// A `content` field as text, spelled once: a string by the caller's text rule; an array's
/// selected parts ([`Parts`]) joined by newlines (empty text parts keep their separators), a
/// single selected part by the text rule; anything else "". Several parts join into `backing`.
pub(crate) fn joined(
    content: Option<crate::json::JsonView<'_>>,
    parts: Parts,
    backing: Backing<'_>,
    text: openai::TextRule<'_>,
) -> anyhow::Result<crate::Text> {
    use super::store::document::Fields;
    let Some(content) = content else {
        return Ok(crate::Text::default());
    };
    if let Some(text) = text(content)? {
        return Ok(text);
    }
    let Some(items) = content.array()? else {
        return Ok(crate::Text::default());
    };
    // Some(None) is an image marker, Some(Some(view)) a string part.
    let keys = ["type", parts.text];
    let mut selected = items.filter_map(|item| {
        let pick = (|| -> anyhow::Result<Option<Option<crate::json::JsonView<'_>>>> {
            let Some(item) = Fields::parse(item?, &keys)? else {
                return Ok(None);
            };
            if parts.image.is_some() && item.string("type")?.as_deref().is_some_and(is_image_part) {
                return Ok(Some(None));
            }
            match item.get(parts.text) {
                Some(value) if value.is_string()? => Ok(Some(Some(value))),
                _ => Ok(None),
            }
        })();
        pick.transpose()
    });
    let marker = parts.image.unwrap_or_default();
    let Some(first) = selected.next().transpose()? else {
        return Ok(crate::Text::default());
    };
    let Some(second) = selected.next().transpose()? else {
        return Ok(match first {
            None => marker.into(),
            Some(view) => text(view)?.unwrap_or_default(),
        });
    };
    let selected = [Ok(first), Ok(second)].into_iter().chain(selected);
    let held = match backing {
        Backing::Over(over) if content.source_range()?.is_some_and(|(_, len)| len > over) => match content.to_json()? {
            crate::Json::Source(range) => Some(range),
            crate::Json::Owned(_) => None,
        },
        _ => None,
    };
    let source = match backing {
        Backing::Source(source) => Some(source),
        _ => held.as_deref().map(crate::json::JsonRange::captured_source),
    };
    match source {
        Some(source) => super::store::text::append(source, &source.decoded_backing()?, |out| {
            for (index, part) in selected.enumerate() {
                if index > 0 {
                    out.push("\n")?;
                }
                match part? {
                    Some(view) => anyhow::ensure!(out.push_string(view)?, "joined string part"),
                    None => out.push(marker)?,
                }
            }
            Ok(())
        }),
        None => {
            let mut joined = String::new();
            for (index, part) in selected.enumerate() {
                if index > 0 {
                    joined.push('\n');
                }
                match part? {
                    Some(view) => view.for_each_str_chunk(|chunk| {
                        joined.push_str(chunk);
                        Ok(())
                    })?,
                    None => {
                        joined.push_str(marker);
                        true
                    }
                };
            }
            Ok(crate::Text::Inline(joined))
        }
    }
}

/// A part is an image whatever the family calls it.
pub(crate) fn is_image_part(ty: &str) -> bool {
    matches!(ty, "image" | "image_url" | "input_image" | "output_image" | "imageUrl")
}

/// An owned `content` value through [`joined`] inline, as a string (tests and owned callers).
pub(crate) fn joined_owned(content: Option<&Value>, parts: Parts) -> String {
    joined(content.map(Into::into), parts, Backing::Inline, &mut |v| v.text(false))
        .expect("an owned value has no source reads")
        .inline_str()
        .expect("inline")
        .to_owned()
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
            let expected = joined_owned(Some(&value), TEXT);
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            for primary_span in [false, true] {
                let source = crate::json::JsonSource::capture(&path)?;
                let text = source.read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                    joined(Some(view), TEXT, Backing::Source(&source), &mut |v| {
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
                    assert!(matches!(text, crate::Text::Source(_)));
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
            let title = source.read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                text_title(Some(view))
            })?;
            let full = joined_owned(Some(&value), TEXT);
            let expected =
                (!full.trim().is_empty()).then(|| crate::ir::truncate(&full, crate::projection::Head::TITLE));
            assert_eq!(title, expected);
        }
        Ok(())
    }
    #[test]
    fn joined_parts_and_markers() {
        let arr = json!([
            {"type": "text", "text": "a"},
            {"type": "image", "source": {"type": "base64", "data": "…"}},
            {"type": "input_text", "text": "b"},
            {"type": "tool_use", "id": "x"}
        ]);
        assert_eq!(joined_owned(Some(&arr), RESULT), "a\n[image]\nb");
        assert_eq!(joined_owned(Some(&arr), TEXT), "a\nb");
        assert_eq!(joined_owned(Some(&json!("plain")), RESULT), "plain");
        assert_eq!(joined_owned(Some(&json!("plain")), TEXT), "plain");
        assert_eq!(joined_owned(None, RESULT), "");
        assert_eq!(joined_owned(Some(&json!({"text": "not an array"})), RESULT), "");
        assert_eq!(joined_owned(Some(&json!([{"type": "image"}])), RESULT), "[image]");
    }
}

#[cfg(test)]
pub(crate) mod genai_frozen;
