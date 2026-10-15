//! Checked wire fields and borrowed ordered-object overlays, not another IR representation.
use crate::{
    harness::store::text,
    lazy::{Resolver, Text},
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value};
use std::{collections::HashMap, io::Write};

pub(crate) struct ObjectWriter<'a> {
    writer: &'a mut dyn Write,
    first: bool,
}
impl<'a> ObjectWriter<'a> {
    pub(crate) fn new(writer: &'a mut dyn Write) -> Result<Self> {
        writer.write_all(b"{")?;
        Ok(Self { writer, first: true })
    }
    pub(crate) fn field(&mut self, key: &str, value: impl Serialize) -> Result<()> {
        self.field_with(key, |writer| serde_json::to_writer(writer, &value).map_err(Into::into))
    }
    pub(crate) fn field_with(&mut self, key: &str, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        if !self.first {
            self.writer.write_all(b",")?;
        }
        self.first = false;
        serde_json::to_writer(&mut *self.writer, key)?;
        self.writer.write_all(b":")?;
        write(self.writer)
    }
    pub(crate) fn finish(self) -> Result<()> {
        self.writer.write_all(b"}")?;
        Ok(())
    }
}
/// Preserve Value's first insertion position and last value, borrowing large replay fields.
pub(crate) struct Object<'a> {
    fields: Vec<(&'a str, &'a Value)>,
    index: Option<HashMap<&'a str, usize>>,
}
impl<'a> Object<'a> {
    pub(crate) fn shortened<'b>(self) -> Object<'b>
    where
        'a: 'b,
    {
        Object {
            fields: self.fields,
            index: self.index,
        }
    }
    pub(crate) fn new(map: &'a Map<String, Value>) -> Self {
        let mut object = Self {
            fields: map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            index: None,
        };
        object.index_large();
        object
    }
    // Tiny envelopes avoid a hash allocation; rich replay maps get linear-total construction.
    fn index_large(&mut self) {
        if self.fields.len() >= 32 && self.index.is_none() {
            self.index = Some(self.fields.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect());
        }
    }
    fn position(&self, key: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.get(key).copied(),
            None => self.fields.iter().position(|(k, _)| *k == key),
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&'a Value> {
        self.position(key).map(|i| self.fields[i].1)
    }
    pub(crate) fn set(&mut self, key: &'a str, value: &'a Value) {
        if let Some(i) = self.position(key) {
            self.fields[i].1 = value;
        } else {
            let i = self.fields.len();
            self.fields.push((key, value));
            if let Some(index) = &mut self.index {
                index.insert(key, i);
            } else {
                self.index_large();
            }
        }
    }
    pub(crate) fn remove(&mut self, key: &str) {
        if let Some(i) = self.position(key) {
            self.fields.remove(i);
            if self.index.is_some() {
                self.index = Some(self.fields.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect());
            }
        }
    }
    pub(crate) fn write(
        &self,
        writer: &mut dyn Write,
        mut field: impl FnMut(&str, &Value, &mut dyn Write) -> Result<bool>,
    ) -> Result<()> {
        let mut object = ObjectWriter::new(writer)?;
        for (k, v) in &self.fields {
            object.field_with(k, |writer| {
                if !field(k, v, writer)? {
                    serde_json::to_writer(writer, v)?;
                }
                Ok(())
            })?;
        }
        object.finish()
    }
}
/// Append checked text to a generated string; return whether decoded text was present.
pub(crate) fn append_text(builder: &mut text::Builder<'_>, value: &Text, resolver: &Resolver) -> Result<bool> {
    let before = builder.decoded_len();
    if let Text::Source(source) = value {
        if source.skip == 0 {
            source.json().with_view(|view| {
                anyhow::ensure!(builder.push_string(view)?, "expected source text string");
                Ok(())
            })?;
            return Ok(builder.decoded_len() != before);
        }
    }
    value.try_for_each_chunk(resolver, 64 * 1024, |chunk| builder.push(chunk))?;
    Ok(builder.decoded_len() != before)
}
pub(crate) fn text(writer: &mut dyn Write, value: &Text, resolver: &Resolver) -> Result<()> {
    if let Some(inline) = value.inline_str() {
        return serde_json::to_writer(writer, inline).map_err(Into::into);
    }
    if let Text::Source(source) = value {
        if source.skip == 0 {
            // One complete string needs no concatenation/counting pass. Canonical publication
            // still checks strict string acceptance and the retained source before/after demand.
            return source.json().with_view(|view| {
                anyhow::ensure!(view.is_string()?, "expected source text string");
                view.write_canonical(&mut &mut *writer)
            });
        }
    }
    text::generate_string(writer, |builder| {
        append_text(builder, value, resolver)?;
        Ok(())
    })
}
/// Exactly Message::text: empty leading parts do not introduce a newline.
pub(crate) fn joined_text<'a>(
    writer: &mut dyn Write,
    values: impl IntoIterator<Item = &'a Text>,
    resolver: &Resolver,
) -> Result<()> {
    text::generate_string(writer, |builder| {
        let mut present = false;
        for value in values {
            if present {
                builder.push("\n")?;
            }
            present |= append_text(builder, value, resolver)?;
        }
        Ok(())
    })
}

/// Preserve direct-emitter source failures even when a target/mode omits a lazy field.
/// Source leaves were strictly validated at capture; recheck provenance and retained backing
/// bounds without decoding their payloads or constructing fidelity features.
pub(crate) fn check_source_fields(session: &crate::ir::Session) -> Result<()> {
    for message in &session.messages {
        if let Some(record) = &message.complete_record {
            if let crate::Json::Source(source) = &record.value {
                source.check_retained_bounds()?;
            }
        }
        for block in &message.content {
            match block {
                crate::ir::Block::Text { text }
                | crate::ir::Block::Thinking { text, .. }
                | crate::ir::Block::ToolResult { content: text, .. } => {
                    if let Text::Source(source) = text {
                        source.range.check_retained_bounds()?;
                    }
                }
                crate::ir::Block::ToolUse {
                    input: crate::Json::Source(source),
                    ..
                } => source.check_retained_bounds()?,
                _ => {}
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct TitleReady;
impl std::fmt::Display for TitleReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("title prefix selected")
    }
}
impl std::error::Error for TitleReady {}

/// The emitters' existing first-user-text fallback, retaining only the truncated label.
/// Like first_user_text this skips empty user turns and applies no prompt filtering.
pub(crate) fn fallback_title(
    session: &crate::ir::Session,
    target: crate::Harness,
    mode: crate::emit::ThinkingMode,
    resolver: &Resolver,
) -> Result<Option<String>> {
    if let Some(title) = &session.title {
        return Ok(Some(crate::ir::truncate(title, 80)));
    }
    for message in session.messages.iter().filter(|m| m.role == crate::ir::Role::User) {
        let mut prefix = String::new();
        let mut chars = 0;
        let mut present = false;
        let mut meaningful = false;
        for block in &message.content {
            let text = if mode == crate::emit::ThinkingMode::Native {
                match block {
                    crate::ir::Block::Text { text } => Some(std::borrow::Cow::Borrowed(text)),
                    _ => None,
                }
            } else {
                crate::emit::thinking::text(block, target, mode, resolver)?
            };
            if let Some(text) = text {
                if present && chars < 81 {
                    prefix.push('\n');
                    chars += 1;
                }
                let result = text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
                    present |= !chunk.is_empty();
                    meaningful = meaningful || chunk.chars().any(|c| !c.is_whitespace());
                    for c in chunk.chars().take(81 - chars) {
                        prefix.push(c);
                        chars += 1;
                    }
                    if chars == 81 && meaningful {
                        return Err(TitleReady.into());
                    }
                    Ok(())
                });
                match result {
                    Err(error) if error.is::<TitleReady>() => break,
                    Err(error) => return Err(error),
                    Ok(()) => {}
                }
            }
        }
        if meaningful {
            return Ok(Some(crate::ir::truncate(&prefix, 80)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod title_tests {
    use super::*;
    #[test]
    fn checked_fallback_title_matches_first_user_text_policy() -> Result<()> {
        use crate::ir::{Block, Message, Role, Session};
        for parts in [
            vec![],
            vec![""],
            vec![" ", "\n"],
            vec!["", "prompt"],
            vec!["first", "", "tail"],
            vec!["\r\nλ", "tail"],
            vec![
                "                                                                               ",
                "late",
            ],
            vec![
                "                                                                                ",
                "late",
            ],
        ] {
            let mut session = Session::new("synthetic", crate::Harness::Continue);
            let mut message = Message::new(Role::User);
            message.content = parts.iter().map(|s| Block::Text { text: (*s).into() }).collect();
            session.messages.push(message);
            let mut later = Message::new(Role::User);
            later.content = vec![Block::Text {
                text: "later meaningful turn".into(),
            }];
            session.messages.push(later);
            let expected = session.first_user_text().map(|s| crate::ir::truncate(&s, 80));
            assert_eq!(
                fallback_title(
                    &session,
                    crate::Harness::Continue,
                    crate::emit::ThinkingMode::Native,
                    &Resolver::new(None)
                )?,
                expected
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod text_publication_tests {
    use super::*;
    use crate::{lazy::SourceText, Harness, Json, Session};

    fn captured(path: &std::path::Path, bytes: &[u8]) -> Result<Text> {
        std::fs::write(path, bytes)?;
        crate::json::JsonSource::capture(path)?.read_syntax(|view| {
            let Json::Source(range) = view.to_json()? else {
                unreachable!()
            };
            // Force Source for empty/tiny strings and syntax-only malformed Unicode so the
            // publication boundary, not only the normal capture factory, proves acceptance.
            Ok(Text::Source(SourceText::new(range)))
        })
    }

    #[test]
    fn canonical_source_text_matches_serde_and_skipped_fallback() -> Result<()> {
        let world = crate::test_world("wire-single-source-string");
        let resolver = Session::new("source", Harness::Claude).resolver();
        for (i, value) in [
            "".to_owned(),
            "plain λ雪".repeat(15000),
            "prefix: escaped\n\"\\\tλ".repeat(6000),
        ]
        .into_iter()
        .enumerate()
        {
            let raw = serde_json::to_vec(&value)?;
            let source = captured(&world.base.join(format!("text-{i}.json")), &raw)?;
            let mut result = Vec::new();
            text(&mut result, &source, &resolver)?;
            assert_eq!(result, serde_json::to_vec(&value)?);
            if i == 2 {
                let skipped = source.with_skipped_prefix(8)?;
                let mut result = Vec::new();
                text(&mut result, &skipped, &resolver)?;
                assert_eq!(result, serde_json::to_vec(&value[8..])?);
            }
        }
        let raw = br#""\u0061\/\u03bb\ud83d\ude00\t\"\\""#;
        let source = captured(&world.base.join("escapes.json"), raw)?;
        let mut result = Vec::new();
        text(&mut result, &source, &resolver)?;
        let expected: String = serde_json::from_slice(raw)?;
        assert_eq!(result, serde_json::to_vec(&expected)?);
        Ok(())
    }

    #[test]
    fn canonical_source_text_rejects_nonstring_and_invalid_unicode() -> Result<()> {
        let world = crate::test_world("wire-invalid-source-string");
        let resolver = Session::new("source", Harness::Claude).resolver();
        for (i, raw) in [b"null".as_slice(), br#""\ud800""#, br#""\udc00""#]
            .into_iter()
            .enumerate()
        {
            let source = captured(&world.base.join(format!("invalid-{i}.json")), raw)?;
            assert!(text(&mut Vec::new(), &source, &resolver).is_err());
        }
        Ok(())
    }

    #[test]
    fn canonical_source_text_checks_identity_even_when_writer_fails() -> Result<()> {
        let world = crate::test_world("wire-source-writer-failure");
        let resolver = Session::new("source", Harness::Claude).resolver();
        let path = world.base.join("source.json");
        struct Fail<'a> {
            source: &'a std::path::Path,
            replacement: Option<&'a std::path::Path>,
        }
        impl Write for Fail<'_> {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                if let Some(replacement) = self.replacement.take() {
                    std::fs::rename(replacement, self.source)?;
                }
                Err(std::io::Error::other("deliberate writer failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let source = captured(&path, br#""checked source""#)?;
        let error = text(
            &mut Fail {
                source: &path,
                replacement: None,
            },
            &source,
            &resolver,
        )
        .unwrap_err();
        assert!(error.to_string().contains("deliberate writer failure"));
        let replacement = world.base.join("replacement.json");
        std::fs::write(&replacement, br#""changed source""#)?;
        let error = text(
            &mut Fail {
                source: &path,
                replacement: Some(&replacement),
            },
            &source,
            &resolver,
        )
        .unwrap_err();
        assert!(error.to_string().contains("source changed"), "{error}");
        assert!(text(&mut Vec::new(), &source, &resolver).is_err());
        Ok(())
    }
}
