//! Read-only document sources and schema-selected fields over them. The byte source and the
//! structural cursor are `range`; strict scalar/depth validation remains in `store::json`.

pub(crate) mod range;

use anyhow::Result;
use serde_json::Value;

/// A derived-envelope shape rejection, distinct from a backing read or source-change error.
#[derive(Debug, thiserror::Error)]
#[error("invalid typed JSON struct: {0}")]
pub(crate) struct InvalidStruct(pub(crate) &'static str);

/// Schema-selected fields across mapped and positioned sources. Structural descendants remain
/// scoped views; only requested metadata/output is materialized. No ignored-key roster is kept.
pub(crate) struct Fields<'a> {
    fields: Vec<(&'static str, crate::json::JsonView<'a>)>,
}
impl<'a> Fields<'a> {
    pub fn parse(view: crate::json::JsonView<'a>, keys: &[&'static str]) -> Result<Option<Self>> {
        Self::parse_with(view, keys, false)
    }
    pub fn parse_unique(view: crate::json::JsonView<'a>, keys: &[&'static str]) -> Result<Option<Self>> {
        Self::parse_with(view, keys, true)
    }
    /// serde's derived struct representation: unique named fields or a positional sequence.
    /// Missing trailing fields are left absent for the caller's defaults; excess fields error.
    pub fn parse_struct(view: crate::json::JsonView<'a>, keys: &[&'static str]) -> Result<Option<Self>> {
        if let Some(fields) = Self::parse_unique(view, keys)? {
            return Ok(Some(fields));
        }
        let Some(values) = view.array()? else { return Ok(None) };
        let mut fields = Vec::with_capacity(keys.len());
        for (index, value) in values.enumerate() {
            let value = value?;
            if index >= keys.len() {
                return Err(InvalidStruct("too many positional fields").into());
            }
            fields.push((keys[index], value));
        }
        Ok(Some(Self { fields }))
    }
    fn parse_with(view: crate::json::JsonView<'a>, keys: &[&'static str], unique: bool) -> Result<Option<Self>> {
        let mut fields: Vec<(&'static str, crate::json::JsonView<'a>)> = Vec::new();
        let mut seen = if unique { vec![false; keys.len()] } else { Vec::new() };
        let complete = view.select_each(keys, unique, &mut |i, value| {
            if unique {
                if seen[i] {
                    return Err(InvalidStruct("duplicate known field").into());
                }
                seen[i] = true;
            }
            match fields.iter_mut().find(|(name, _)| *name == keys[i]) {
                Some(field) => field.1 = value,
                None => fields.push((keys[i], value)),
            }
            Ok(())
        })?;
        Ok(complete.then_some(Self { fields }))
    }
    pub fn get(&self, key: &str) -> Option<crate::json::JsonView<'a>> {
        self.fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    }
    pub fn value(&self, key: &str) -> Result<Option<Value>> {
        self.get(key).map(|view| Ok(view.resolve()?.into_owned())).transpose()
    }
    pub fn string(&self, key: &str) -> Result<Option<String>> {
        self.get(key).map(|view| view.string()).transpose().map(Option::flatten)
    }
    pub fn project(&self, keys: &[&str]) -> Result<serde_json::Map<String, Value>> {
        keys.iter()
            .filter_map(|key| self.get(key).map(|view| (*key, view)))
            .map(|(key, view)| Ok((key.to_owned(), view.resolve()?.into_owned())))
            .collect()
    }
}

/// Decode only the requested characters after optional leading whitespace. Accepted source
/// strings retain their complete capture validation and recheck provenance at the early stop.
pub(crate) fn text_head(
    text: &crate::Text,
    resolver: &crate::lazy::Resolver,
    max_chars: usize,
    trim_leading: bool,
) -> Result<(String, bool)> {
    let mut head = String::new();
    let mut chars = 0;
    let mut present = false;
    match text.try_for_each_chunk(resolver, 4096, |chunk| {
        present |= !chunk.is_empty();
        for ch in chunk.chars() {
            if trim_leading && head.is_empty() && ch.is_whitespace() {
                continue;
            }
            if chars == max_chars {
                return Err(crate::stream::Stop.into());
            }
            head.push(ch);
            chars += 1;
            if chars == max_chars {
                return Err(crate::stream::Stop.into());
            }
        }
        Ok(())
    }) {
        Ok(()) => {}
        Err(error) if error.is::<crate::stream::Stop>() => {}
        Err(error) => return Err(error),
    }
    Ok((head, present))
}

/// The same joined text/title as Message::text, as a title [`Head`](crate::projection::Head). When
/// discovery filters whitespace-only text, scan any remaining whitespace with bounded state.
pub(crate) fn text_summary<T: std::borrow::Borrow<crate::lazy::Text>>(
    texts: impl Iterator<Item = T>,
    resolver: &crate::lazy::Resolver,
    require_nonwhite: bool,
) -> Option<String> {
    let mut head = crate::projection::Head::bounded(crate::projection::Head::TITLE);
    let mut present = false;
    for (index, text) in texts.enumerate() {
        let text = text.borrow();
        if present || (require_nonwhite && index > 0) {
            head.push("\n");
        }
        match text {
            crate::lazy::Text::Inline(s) => {
                present |= !s.is_empty();
                head.push(s);
            }
            crate::lazy::Text::Source(_) | crate::lazy::Text::Span(_) => {
                // Discovery titles are optional hints: failed text reads contribute no prefix.
                // Transcript consumers use checked reads and report source failures separately.
                let prefix = text.try_resolve_prefix(resolver, 1024).unwrap_or_default();
                present |= !prefix.is_empty();
                head.push(&prefix);
                if require_nonwhite && head.is_blank() && text.len() > 1024 {
                    let _ = text.try_for_each_chunk(resolver, 4096, |chunk| {
                        head.push(chunk);
                        Ok(())
                    });
                }
            }
        }
        if head.full() && (!require_nonwhite || !head.is_blank()) {
            break;
        }
    }
    (present && (!require_nonwhite || !head.is_blank())).then(|| head.finish())
}

/// Unicode whitespace classification without materializing a retained payload.
#[cfg(feature = "sqlite")]
pub(crate) fn text_is_blank(text: &crate::Text, resolver: &crate::Resolver) -> Result<bool> {
    Ok(text_head(text, resolver, 1, true)?.0.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_text_heads_preserve_classification_boundaries_and_source_errors() -> Result<()> {
        let world = crate::test_world("checked-text-heads");
        let path = world.base.join("text.json");
        let resolver = crate::lazy::Resolver::new(None);
        for body in [
            String::new(),
            " ".into(),
            "\n\u{2003}".repeat(8192),
            format!("{}<user_info>\nλ followed by payload", " \n\u{2003}".repeat(8192)),
            format!("{}<qwen:user-prompt-submit-context>hook", " \n\u{2003}".repeat(8192)),
            format!("{}\nλ tail", "界".repeat(80)),
            format!("\\n{}", "λ".repeat(8192)),
        ] {
            let bytes = serde_json::to_vec(&body)?;
            std::fs::write(&path, &bytes)?;
            let crate::Json::Source(range) = crate::Json::source(&path, 0, bytes.len() as u64)? else {
                unreachable!()
            };
            let text = crate::Text::Source(crate::lazy::SourceText::new(range));
            for max_chars in [1, 16, 80] {
                for trim in [false, true] {
                    let expected: String = if trim { body.trim_start() } else { &body }
                        .chars()
                        .take(max_chars)
                        .collect();
                    assert_eq!(
                        text_head(&text, &resolver, max_chars, trim)?,
                        (expected, !body.is_empty())
                    );
                }
            }
            std::fs::write(&path, b"changed source")?;
            assert!(text_head(&text, &resolver, 16, true).is_err());
            assert!(text_head(&text, &resolver, 80, false).is_err());
        }
        let malformed = br#""usable prefix followed by \ud800""#;
        std::fs::write(&path, malformed)?;
        assert!(crate::Json::source(&path, 0, malformed.len() as u64).is_err());
        Ok(())
    }
}
