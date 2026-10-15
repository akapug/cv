//! Read-only document sources and borrowed structural slices. Strict scalar/depth validation
//! remains in `store::json`; these indexes describe the bytes rather than retain their payloads.

// Native bounded prefixes also use the positioned string reader; full document traversal
// remains portable-only, so those methods are intentionally unused in native production.
#[cfg_attr(feature = "mmap", allow(dead_code))]
pub(crate) mod range;

use crate::lazy::INLINE_MAX;
#[cfg(feature = "mmap")]
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

/// Read-only mapped source bytes used by native scoped views and JSONL readers.
#[cfg(feature = "mmap")]
pub(crate) struct Source {
    bytes: Bytes,
    index: Index,
}
#[cfg(feature = "mmap")]
enum Bytes {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}
#[cfg(feature = "mmap")]
impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(m) => m,
            Self::Owned(v) => v,
        }
    }
}
#[cfg(feature = "mmap")]
impl Source {
    /// Map an existing read-only descriptor. JSONL readers retain it for selected-field replay.
    pub fn map_file(file: &std::fs::File) -> Result<Self> {
        // External truncation has the same documented caveat as the other mapped adapters.
        let bytes = if file.metadata()?.len() == 0 {
            Bytes::Owned(Vec::new())
        } else {
            Bytes::Mapped(unsafe { memmap2::Mmap::map(file) }.context("mapping source")?)
        };
        Ok(Self {
            bytes,
            index: Index::default(),
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn slice(&self, offset: u64, len: u64) -> Option<Raw<'_>> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        let text = std::str::from_utf8(self.bytes.get(start..end)?).ok()?;
        Some(Raw::new(text, &self.index))
    }
}

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
        let Some(view) = self.get(key) else { return Ok(None) };
        if !view.is_string()? {
            return Ok(None);
        }
        match view.resolve()?.into_owned() {
            Value::String(s) => Ok(Some(s)),
            _ => unreachable!(),
        }
    }
    pub fn project(&self, keys: &[&str]) -> Result<serde_json::Map<String, Value>> {
        keys.iter()
            .filter_map(|key| self.get(key).map(|view| (*key, view)))
            .map(|(key, view)| Ok((key.to_owned(), view.resolve()?.into_owned())))
            .collect()
    }
}

/// Slices inside a RawValue already validated by serde at the file/line boundary. Walking a
/// shallow object must not repeatedly validate its potentially enormous string descendants.
#[derive(Clone, Copy)]
pub(crate) struct Raw<'a>(&'a str, &'a Index);

/// Containers longer than this (and strings over INLINE_MAX) are remembered while validating,
/// so each later structural selection (record, message, content, block) skips them in O(1).
const SKIP_MIN: usize = 128;

/// Memoize at most 1024 recent long string endpoints. Nested record wrappers reuse body bounds
/// without retaining offsets for the entire history. Eviction affects only scan cost, not data.
const ENDPOINTS: usize = 1024;
#[derive(Default)]
pub(crate) struct Index(std::cell::RefCell<Endpoints>);
#[derive(Default)]
struct Endpoints {
    ends: BTreeMap<usize, usize>,
    order: VecDeque<usize>,
}
impl Index {
    fn get(&self, key: usize) -> Option<usize> {
        self.0.borrow().ends.get(&key).copied()
    }
    fn remember(&self, start: usize, end: usize) {
        let mut cache = self.0.borrow_mut();
        if cache.ends.contains_key(&start) {
            return;
        }
        if cache.ends.len() == ENDPOINTS {
            if let Some(old) = cache.order.pop_front() {
                cache.ends.remove(&old);
            }
        }
        cache.ends.insert(start, end);
        cache.order.push_back(start);
    }
}
impl<'a> Raw<'a> {
    /// `raw` must be a serde-validated RawValue or a slice returned by this module.
    pub fn new(raw: &'a str, index: &'a Index) -> Self {
        Self(raw, index)
    }
    pub fn get(self) -> &'a str {
        self.0
    }
}

/// Whether an unvalidated slice is exactly a serde `Value`, in one pass that also indexes it.
pub(crate) fn validate(raw: Raw<'_>) -> bool {
    let b = raw.get().as_bytes();
    super::json::validate_value(raw.get(), |start, end| {
        if end - start > if b[start] == b'"' { INLINE_MAX } else { SKIP_MIN } {
            raw.1.remember(b.as_ptr() as usize + start, b.as_ptr() as usize + end);
        }
    })
}

pub(crate) fn accepts_value(raw: Raw<'_>) -> bool {
    accepts_value_at_depth(raw, 0)
}

pub(crate) fn accepts_value_at_depth(raw: Raw<'_>, depth: usize) -> bool {
    let b = raw.get().as_bytes();
    super::json::value_compatible_str_at_depth_with(raw.get(), depth, |start, end| {
        // memchr already skips a string quickly; only a long one is worth an index entry.
        if end - start > if b[start] == b'"' { INLINE_MAX } else { SKIP_MIN } {
            raw.1.remember(b.as_ptr() as usize + start, b.as_ptr() as usize + end);
        }
    })
}

/// Select a bounded schema roster without allocating ignored keys or descendant payloads.
/// Keys obey Value's final-duplicate rule; complete acceptance remains the caller's boundary.
pub(crate) fn select_fields<'a>(raw: Raw<'a>, keys: &[&str]) -> Option<Vec<Option<Raw<'a>>>> {
    select_fields_with(raw, keys, false).ok().flatten()
}
fn select_fields_with<'a>(raw: Raw<'a>, keys: &[&str], unique: bool) -> Result<Option<Vec<Option<Raw<'a>>>>> {
    let mut fields = vec![None; keys.len()];
    let complete = select_each(raw, keys, unique, &mut |i, value| {
        if unique && fields[i].is_some() {
            return Err(InvalidStruct("duplicate known field").into());
        }
        fields[i] = Some(value);
        Ok(())
    })?;
    Ok(complete.then_some(fields))
}
pub(crate) fn select_unique_fields<'a>(raw: Raw<'a>, keys: &[&str]) -> Result<Option<Vec<Option<Raw<'a>>>>> {
    select_fields_with(raw, keys, true)
}
/// Visit each requested field of an object in wire order (a later duplicate visits again, so
/// the last visit wins). False for a nonobject or malformed object, whose visits must be dropped.
pub(crate) fn select_each<'a>(
    raw: Raw<'a>,
    keys: &[&str],
    unique: bool,
    visit: &mut dyn FnMut(usize, Raw<'a>) -> Result<()>,
) -> Result<bool> {
    use anyhow::Context;
    let b = raw.get().as_bytes();
    if b.first() != Some(&b'{') {
        return Ok(false);
    }
    let key_limit = keys
        .iter()
        .map(|key| key.len())
        .max()
        .unwrap_or(0)
        .saturating_mul(6)
        .saturating_add(2);
    let mut cursor = 1;
    loop {
        whitespace(b, &mut cursor);
        if b.get(cursor) == Some(&b'}') {
            return Ok(true);
        }
        let start = cursor;
        cursor = string_end(b, start, raw.1).context("invalid JSON key")?;
        if unique && !super::json::value_compatible(&b[start..cursor]) {
            return Err(InvalidStruct("invalid object key").into());
        }
        let quoted = &raw.get()[start..cursor];
        let key = if cursor - start > key_limit {
            None
        } else if !quoted.as_bytes().contains(&b'\\') {
            Some(std::borrow::Cow::Borrowed(&quoted[1..quoted.len() - 1]))
        } else {
            serde_json::from_str::<String>(quoted).ok().map(std::borrow::Cow::Owned)
        };
        whitespace(b, &mut cursor);
        if b.get(cursor) != Some(&b':') {
            return Ok(false);
        }
        cursor += 1;
        whitespace(b, &mut cursor);
        let start = cursor;
        cursor = value_end(b, start, raw.1).context("invalid JSON field")?;
        if let Some(key) = key {
            for (i, requested) in keys.iter().enumerate() {
                if key == **requested {
                    visit(i, Raw(&raw.get()[start..cursor], raw.1))?;
                }
            }
        }
        whitespace(b, &mut cursor);
        match b.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => return Ok(true),
            _ => return Ok(false),
        }
    }
}

/// Syntax-validated object entries in wire order, including duplicate names.
struct ObjectIter<'a> {
    raw: Raw<'a>,
    cursor: usize,
    done: bool,
}
impl<'a> Iterator for ObjectIter<'a> {
    type Item = (std::borrow::Cow<'a, str>, Raw<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let b = self.raw.get().as_bytes();
        whitespace(b, &mut self.cursor);
        if b.get(self.cursor) == Some(&b'}') {
            self.done = true;
            return None;
        }
        let start = self.cursor;
        self.cursor = string_end(b, start, self.raw.1)?;
        let quoted = &self.raw.get()[start..self.cursor];
        let key = if quoted.as_bytes().contains(&b'\\') {
            std::borrow::Cow::Owned(serde_json::from_str(quoted).ok()?)
        } else {
            std::borrow::Cow::Borrowed(&quoted[1..quoted.len() - 1])
        };
        whitespace(b, &mut self.cursor);
        if b.get(self.cursor) != Some(&b':') {
            return None;
        }
        self.cursor += 1;
        whitespace(b, &mut self.cursor);
        let start = self.cursor;
        self.cursor = value_end(b, start, self.raw.1)?;
        let value = Raw(&self.raw.get()[start..self.cursor], self.raw.1);
        whitespace(b, &mut self.cursor);
        match b.get(self.cursor) {
            Some(b',') => self.cursor += 1,
            Some(b'}') => self.done = true,
            _ => return None,
        }
        Some((key, value))
    }
}
pub(crate) struct Record<'a> {
    pub fields: BTreeMap<String, Raw<'a>>,
}
impl<'a> Record<'a> {
    /// Missing/nonobject nested fields behave as an empty object without allocating a Value.
    pub fn empty(_: Raw<'a>) -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }
    pub fn parse(raw: Raw<'a>) -> Option<Self> {
        if !raw.get().starts_with('{') {
            return None;
        }
        let mut entries = ObjectIter {
            raw,
            cursor: 1,
            done: false,
        };
        let fields = entries.by_ref().map(|(key, value)| (key.into_owned(), value)).collect();
        entries.done.then_some(Self { fields })
    }
    pub fn get(&self, key: &str) -> Option<Raw<'a>> {
        self.fields.get(key).copied()
    }
    #[cfg(test)]
    pub fn value(&self, key: &str) -> Option<Value> {
        serde_json::from_str(self.get(key)?.get()).ok()
    }
    pub fn string(&self, key: &str) -> Option<String> {
        serde_json::from_str(self.fields.get(key)?.get()).ok()
    }
}

/// An object's entries as Value keeps them: each key at its first position, with its final
/// value. None for a nonobject or malformed object. Narrow objects dedupe by scanning; wide
/// ones by a position map.
pub(crate) fn object_entries(raw: Raw<'_>) -> Option<Vec<(std::borrow::Cow<'_, str>, Raw<'_>)>> {
    if !raw.get().starts_with('{') {
        return None;
    }
    let mut iter = ObjectIter {
        raw,
        cursor: 1,
        done: false,
    };
    let mut entries: Vec<(std::borrow::Cow<'_, str>, Raw<'_>)> = Vec::new();
    let mut positions: Option<std::collections::HashMap<String, usize>> = None;
    for (key, value) in iter.by_ref() {
        if entries.len() == 16 && positions.is_none() {
            positions = Some(
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (k.to_string(), i))
                    .collect(),
            );
        }
        let existing = match &positions {
            Some(positions) => positions.get(key.as_ref()).copied(),
            None => entries.iter().position(|(k, _)| *k == key),
        };
        match existing {
            Some(i) => entries[i].1 = value,
            None => {
                if let Some(positions) = &mut positions {
                    positions.insert(key.to_string(), entries.len());
                }
                entries.push((key, value));
            }
        }
    }
    iter.done.then_some(entries)
}

/// Chronological traversal keeps one descriptor regardless of record count. Input must already
/// be syntax validated, as with every structural slice operation in this module.
#[derive(Clone, Copy)]
pub(crate) struct ArrayIter<'a> {
    raw: Raw<'a>,
    cursor: usize,
    done: bool,
}
pub(crate) fn array_iter(raw: Raw<'_>) -> Option<ArrayIter<'_>> {
    raw.get().starts_with('[').then_some(ArrayIter {
        raw,
        cursor: 1,
        done: false,
    })
}
impl<'a> Iterator for ArrayIter<'a> {
    type Item = Raw<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let bytes = self.raw.get().as_bytes();
        whitespace(bytes, &mut self.cursor);
        if bytes.get(self.cursor) == Some(&b']') {
            self.done = true;
            return None;
        }
        let start = self.cursor;
        let end = value_end(bytes, start, self.raw.1)?;
        self.cursor = end;
        whitespace(bytes, &mut self.cursor);
        match bytes.get(self.cursor) {
            Some(b',') => self.cursor += 1,
            Some(b']') => self.done = true,
            _ => {
                self.done = true;
                return None;
            }
        }
        Some(Raw(&self.raw.get()[start..end], self.raw.1))
    }
}

fn whitespace(b: &[u8], i: &mut usize) {
    while b.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}
fn string_end(b: &[u8], i: usize, index: &Index) -> Option<usize> {
    let key = b.as_ptr() as usize + i;
    if let Some(end) = index.get(key) {
        return Some(end - b.as_ptr() as usize);
    }
    let end = super::json::string_end(b, i)?;
    if end - i > INLINE_MAX {
        index.remember(key, b.as_ptr() as usize + end);
    }
    Some(end)
}
fn value_end(b: &[u8], mut i: usize, index: &Index) -> Option<usize> {
    match b.get(i)? {
        b'"' => string_end(b, i, index),
        b'{' | b'[' => {
            if let Some(end) = index.get(b.as_ptr() as usize + i) {
                return Some(end - b.as_ptr() as usize);
            }
            let mut depth = 1usize;
            i += 1;
            while depth != 0 {
                match b.get(i)? {
                    b'"' => {
                        i = string_end(b, i, index)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            Some(i)
        }
        _ => {
            let start = i;
            while b
                .get(i)
                .is_some_and(|c| !c.is_ascii_whitespace() && !b",]}".contains(c))
            {
                i += 1;
            }
            (i > start).then_some(i)
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("requested text head complete")]
struct HeadComplete;

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
                return Err(HeadComplete.into());
            }
            head.push(ch);
            chars += 1;
            if chars == max_chars {
                return Err(HeadComplete.into());
            }
        }
        Ok(())
    }) {
        Ok(()) => {}
        Err(error) if error.is::<HeadComplete>() => {}
        Err(error) => return Err(error),
    }
    Ok((head, present))
}

/// The same joined text/title as Message::text, retaining only the first 81 characters. When
/// discovery filters whitespace-only text, scan any remaining whitespace with bounded state.
pub(crate) fn text_summary<T: std::borrow::Borrow<crate::lazy::Text>>(
    texts: impl Iterator<Item = T>,
    resolver: &crate::lazy::Resolver,
    require_nonwhite: bool,
) -> Option<String> {
    let mut head = String::new();
    let mut chars = 0;
    let mut present = false;
    let mut nonwhite = false;
    for (index, text) in texts.enumerate() {
        let text = text.borrow();
        if (present || (require_nonwhite && index > 0)) && chars < 81 {
            head.push('\n');
            chars += 1;
        }
        let mut feed = |chunk: &str| {
            present |= !chunk.is_empty();
            nonwhite |= chunk.chars().any(|c| !c.is_whitespace());
            for c in chunk.chars().take(81 - chars) {
                head.push(c);
                chars += 1;
            }
        };
        match text {
            crate::lazy::Text::Inline(s) => feed(s),
            crate::lazy::Text::Source(_) | crate::lazy::Text::Span(_) => {
                let prefix = text.try_resolve_prefix(resolver, 1024).unwrap_or_default();
                feed(&prefix);
                if require_nonwhite && !nonwhite && text.len() > 1024 {
                    let _ = text.try_for_each_chunk(resolver, 4096, |chunk| {
                        nonwhite |= chunk.chars().any(|c| !c.is_whitespace());
                        Ok(())
                    });
                }
            }
        }
        if chars == 81 && (!require_nonwhite || nonwhite) {
            break;
        }
    }
    (present && (!require_nonwhite || nonwhite)).then(|| crate::ir::truncate(&head, 80))
}

#[cfg(test)]
mod tests {
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

    use super::*;

    #[test]
    fn shallow_slices_preserve_validated_json_semantics() {
        for input in [
            r#"{"a":1,"a":2,"\u0061":3,"comma,key":{"q":"escaped\"quote\\slash","a":[1,-2.5e+3,true,false,null,[],{}]},"尾":"🌙"}"#,
            "{ \n \"a\" : [ 1e-5, {\"a\": \"\\uD83C\\uDF19\"}, \"end\" ], \"z\" : null }",
        ] {
            let validated: &crate::lazy::RawValue = serde_json::from_str(input).unwrap();
            let expected: Value = serde_json::from_str(input).unwrap();
            let index = Index::default();
            let actual = Record::parse(Raw::new(validated.get(), &index)).unwrap();
            assert_eq!(actual.fields.len(), expected.as_object().unwrap().len());
            for (key, raw) in actual.fields {
                let value: Value = serde_json::from_str(raw.get()).unwrap();
                assert_eq!(value, expected[&key]);
                if let Some(items) = array_iter(raw).map(Iterator::collect::<Vec<_>>) {
                    assert_eq!(items.len(), value.as_array().unwrap().len());
                    for (raw, expected) in items.into_iter().zip(value.as_array().unwrap()) {
                        assert_eq!(serde_json::from_str::<Value>(raw.get()).unwrap(), *expected);
                    }
                }
            }
        }
        for input in ["null", "true", "123", "[]", "\"string\""] {
            let index = Index::default();
            let raw = Raw::new(input, &index);
            assert!(Record::parse(raw).is_none());
            let empty = Record::empty(raw);
            assert!(empty.fields.is_empty());
        }
    }
    #[test]
    fn array_iteration_and_endpoint_eviction_preserve_records() {
        let body = "a\\\"x".repeat(2048);
        let values = (0..ENDPOINTS * 3)
            .map(|n| serde_json::json!({"n":n,"body":body}))
            .collect::<Vec<_>>();
        let text = serde_json::to_string(&values).unwrap();
        let validated: &crate::lazy::RawValue = serde_json::from_str(&text).unwrap();
        let index = Index::default();
        let raw = Raw::new(validated.get(), &index);
        assert!(accepts_value(raw));
        assert!(index.0.borrow().ends.len() <= ENDPOINTS);
        let iterator = array_iter(raw).unwrap();
        assert_eq!(iterator.count(), values.len());
        for (raw, expected) in iterator.zip(values.iter()) {
            let record = Record::parse(raw).unwrap();
            assert_eq!(record.value("n").unwrap(), expected["n"]);
            assert_eq!(record.value("body").unwrap(), expected["body"]);
            assert!(index.0.borrow().ends.len() <= ENDPOINTS);
        }
    }
}
