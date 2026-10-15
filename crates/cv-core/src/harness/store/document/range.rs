//! Positioned, bounded input windows for source-backed JSON. Offsets count consumed bytes, never
//! underlying reads or serde lookahead. The same structural scanner works on bytes and files.

use super::super::bytes::read_at;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{self, BufRead, Read};
#[cfg(test)]
use std::path::Path;

const WINDOW: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub offset: u64,
    pub len: u64,
}
impl Range {
    fn end(self) -> io::Result<u64> {
        self.offset
            .checked_add(self.len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source range overflows"))
    }
}

/// One readonly descriptor shared by all projected fields in an operation. Independent cursors
/// use positioned reads so nested views do not share seek position or hold a RefCell borrow.
pub(crate) struct RangeSource {
    file: Option<File>,
    memory: Option<std::sync::Arc<crate::harness::store::bytes::ByteSource>>,
    captured: Option<crate::json::JsonSource>,
    decoded_len: Option<u64>,
    // Reuse the native bounded long-string endpoint index only within this source operation.
    index: super::Index,
}
impl RangeSource {
    /// Immutable operation-owned bytes need no external source identity or pathname.
    pub(crate) fn private(bytes: std::sync::Arc<crate::harness::store::bytes::ByteSource>) -> Result<Self> {
        let len = bytes.len()?;
        let (file, memory) = if bytes.memory_bytes().is_some() {
            (None, Some(bytes))
        } else {
            (Some(bytes.file()?), None)
        };
        Ok(Self {
            file,
            memory,
            captured: None,
            decoded_len: Some(len),
            index: super::Index::default(),
        })
    }
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_captured(crate::json::JsonSource::capture(path)?)
    }
    pub fn from_captured(captured: crate::json::JsonSource) -> Result<Self> {
        let file = File::open(captured.path()).with_context(|| format!("opening {}", captured.path().display()))?;
        Self::from_file(file, captured)
    }
    /// Reuse an already opened original-source descriptor for framing and selected reads.
    pub(crate) fn from_file(file: File, captured: crate::json::JsonSource) -> Result<Self> {
        let source = Self {
            file: Some(file),
            memory: None,
            captured: Some(captured),
            decoded_len: None,
            index: super::Index::default(),
        };
        source.check_identity()?;
        Ok(source)
    }
    pub(crate) fn decoded(
        captured: crate::json::JsonSource,
        bytes: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
    ) -> Result<Self> {
        let decoded_len = Some(bytes.len()?);
        let (file, memory) = if bytes.memory_bytes().is_some() {
            (None, Some(bytes.clone()))
        } else {
            (Some(bytes.file()?), None)
        };
        let source = Self {
            file,
            memory,
            captured: Some(captured),
            decoded_len,
            index: super::Index::default(),
        };
        source.check_identity()?;
        Ok(source)
    }
    pub(crate) fn matches_original(&self, captured: &crate::json::JsonSource) -> bool {
        self.decoded_len.is_none() && self.captured.as_ref() == Some(captured)
    }
    pub fn range(&self) -> Range {
        Range {
            offset: 0,
            len: self
                .decoded_len
                .unwrap_or_else(|| self.captured.as_ref().unwrap().len()),
        }
    }
    pub fn check_identity(&self) -> Result<()> {
        let Some(captured) = &self.captured else { return Ok(()) };
        if self.decoded_len.is_none() {
            captured.check_metadata(self.file.as_ref().unwrap().metadata()?)?;
        }
        captured.check()
    }
    pub fn cursor(&self, range: Range) -> io::Result<Cursor<'_>> {
        if range.end()? > self.range().end()? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "range extends beyond source",
            ));
        }
        Ok(Cursor {
            backing: match &self.memory {
                Some(bytes) => Backing::Bytes(bytes.memory_bytes().unwrap(), 0),
                None => Backing::File(self.file.as_ref().unwrap()),
            },
            index: Some(&self.index),
            range,
            position: range.offset,
            window: Vec::new(),
            window_start: range.offset,
            read_error: None,
        })
    }
}

enum Backing<'a> {
    Bytes(&'a [u8], u64),
    File(&'a File),
}
pub(crate) struct Cursor<'a> {
    backing: Backing<'a>,
    index: Option<&'a super::Index>,
    range: Range,
    position: u64,
    window: Vec<u8>,
    window_start: u64,
    read_error: Option<io::ErrorKind>,
}
impl<'a> Cursor<'a> {
    #[cfg(test)]
    pub fn bytes(bytes: &'a [u8], base: u64) -> io::Result<Self> {
        let range = Range {
            offset: base,
            len: bytes.len() as u64,
        };
        range.end()?;
        Ok(Self {
            backing: Backing::Bytes(bytes, base),
            index: None,
            range,
            position: base,
            window: Vec::new(),
            window_start: base,
            read_error: None,
        })
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn peek(&mut self) -> io::Result<Option<u8>> {
        Ok(self.fill_buf()?.first().copied())
    }
    pub fn whitespace(&mut self) -> io::Result<()> {
        loop {
            let bytes = self.fill_buf()?;
            let n = bytes.iter().take_while(|b| b.is_ascii_whitespace()).count();
            if n == 0 {
                return Ok(());
            }
            self.consume(n);
        }
    }
    pub fn borrow_range(&self, range: Range) -> Option<&[u8]> {
        let (bytes, base) = match &self.backing {
            Backing::Bytes(b, base) => (*b, *base),
            Backing::File(_) => (self.window.as_slice(), self.window_start),
        };
        let start = usize::try_from(range.offset.checked_sub(base)?).ok()?;
        let end = start.checked_add(usize::try_from(range.len).ok()?)?;
        bytes.get(start..end)
    }
    /// Only structural navigation; the caller validates grammar and scalar acceptance once at
    /// the document/record boundary. Escapes may span windows without decoding their payload.
    pub fn string(&mut self) -> io::Result<Range> {
        let start = self.position();
        if self.peek()? != Some(b'"') {
            return Err(invalid("expected JSON string"));
        }
        // Absolute source offsets share the native index representation where they fit.
        // A narrower child cursor never jumps beyond its own accepted extent.
        if let (Some(index), Ok(key)) = (self.index, usize::try_from(start)) {
            if let Some(end) = index.get(key) {
                let end = end as u64;
                if end > start && end <= self.range.end()? {
                    self.position = end;
                    return Ok(Range {
                        offset: start,
                        len: end - start,
                    });
                }
            }
        }
        self.consume(1);
        // Only quotes can end a string. Carry slash parity across refill boundaries so
        // dense escaped bodies remain vectorized rather than visiting every backslash.
        let mut trailing_odd = false;
        loop {
            let bytes = self.fill_buf()?;
            if bytes.is_empty() {
                return Err(invalid("unterminated JSON string"));
            }
            let Some(n) = memchr::memchr(b'"', bytes) else {
                let trailing = bytes.iter().rev().take_while(|&&b| b == b'\\').count();
                trailing_odd = if trailing == bytes.len() {
                    trailing_odd ^ (trailing % 2 != 0)
                } else {
                    trailing % 2 != 0
                };
                let n = bytes.len();
                self.consume(n);
                continue;
            };
            let slashes = bytes[..n].iter().rev().take_while(|&&b| b == b'\\').count();
            let escaped = (slashes % 2 != 0) ^ (slashes == n && trailing_odd);
            self.consume(n + 1);
            if !escaped {
                if self.position - start > crate::lazy::INLINE_MAX as u64 {
                    if let (Some(index), Ok(start), Ok(end)) =
                        (self.index, usize::try_from(start), usize::try_from(self.position))
                    {
                        index.remember(start, end);
                    }
                }
                return Ok(Range {
                    offset: start,
                    len: self.position - start,
                });
            }
            trailing_odd = false;
        }
    }

    pub fn value(&mut self) -> io::Result<Range> {
        self.whitespace()?;
        let start = self.position();
        match self.peek()? {
            Some(b'"') => return self.string(),
            Some(b'{' | b'[') => {
                self.consume(1);
                let mut depth = 1usize;
                while depth != 0 {
                    match self.peek()? {
                        Some(b'"') => {
                            self.string()?;
                        }
                        Some(b'{' | b'[') => {
                            depth = depth.checked_add(1).ok_or_else(|| invalid("nesting overflow"))?;
                            self.consume(1);
                        }
                        Some(b'}' | b']') => {
                            depth -= 1;
                            self.consume(1);
                        }
                        Some(_) => {
                            self.consume(1);
                        }
                        None => return Err(invalid("unterminated JSON container")),
                    }
                }
            }
            Some(_) => loop {
                let bytes = self.fill_buf()?;
                let n = bytes
                    .iter()
                    .take_while(|b| !b.is_ascii_whitespace() && !b",]}".contains(b))
                    .count();
                if n == 0 {
                    break;
                }
                self.consume(n);
            },
            None => return Err(invalid("expected JSON value")),
        }
        if self.position == start {
            return Err(invalid("expected JSON value"));
        }
        Ok(Range {
            offset: start,
            len: self.position - start,
        })
    }
}
impl BufRead for Cursor<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position == self.range.end()? {
            return Ok(&[]);
        }
        match self.backing {
            Backing::Bytes(bytes, base) => {
                Ok(&bytes[(self.position - base) as usize..(self.range.end()? - base) as usize])
            }
            Backing::File(file) => {
                let consumed = usize::try_from(self.position - self.window_start).unwrap_or(usize::MAX);
                if consumed < self.window.len() {
                    return Ok(&self.window[consumed..]);
                }
                self.window_start = self.position;
                let n = (self.range.end()? - self.position).min(WINDOW as u64) as usize;
                self.window.resize(n, 0);
                let n = loop {
                    match read_at(file, &mut self.window, self.position) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            self.read_error = Some(e.kind());
                            return Err(e);
                        }
                        Ok(n) => break n,
                    }
                };
                if n == 0 {
                    self.read_error = Some(io::ErrorKind::UnexpectedEof);
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "source was truncated"));
                }
                self.window.truncate(n);
                Ok(&self.window)
            }
        }
    }
    fn consume(&mut self, n: usize) {
        assert!(
            n as u64 <= self.range.end().unwrap() - self.position,
            "cursor consumed beyond range"
        );
        self.position += n as u64;
    }
}
impl Read for Cursor<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let bytes = self.fill_buf()?;
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        self.consume(n);
        Ok(n)
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
/// A grammar-only view: quoted tokens are independently validated in bounded windows, then
/// represented by an empty string. The original source and all consumer values remain intact.
/// Keeping punctuation, numbers and literals lets serde retain authority over JSON grammar.
struct SyntaxReader<'a> {
    source: &'a RangeSource,
    cursor: Cursor<'a>,
    plain_left: usize,
    quotes_left: usize,
    invalid_string: bool,
    strict_strings: bool,
}
impl<'a> SyntaxReader<'a> {
    fn new(source: &'a RangeSource, range: Range) -> io::Result<Self> {
        Ok(Self {
            source,
            cursor: source.cursor(range)?,
            plain_left: 0,
            quotes_left: 0,
            invalid_string: false,
            strict_strings: false,
        })
    }
}
impl Read for SyntaxReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.quotes_left > 0 {
            let n = self.quotes_left.min(output.len());
            output[..n].fill(b'"');
            self.quotes_left -= n;
            return Ok(n);
        }
        if self.plain_left == 0 {
            let bytes = self.cursor.fill_buf()?;
            if bytes.is_empty() {
                return Ok(0);
            }
            self.plain_left = memchr::memchr(b'"', bytes).unwrap_or(bytes.len());
            if self.plain_left == 0 {
                let string = match self.cursor.string() {
                    Ok(string) => string,
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                        self.invalid_string = true;
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                };
                let valid = if let Some(bytes) = self.cursor.borrow_range(string) {
                    serde_json::from_slice::<&serde_json::value::RawValue>(bytes).is_ok()
                        && (!self.strict_strings || super::super::json::value_compatible(bytes))
                } else {
                    let mut body = self.source.cursor(Range {
                        offset: string.offset + 1,
                        len: string.len - 2,
                    })?;
                    let valid = if self.strict_strings {
                        super::super::json::validate_body(&mut body, true).is_ok()
                    } else {
                        super::super::json::validate_body(&mut body, false).is_ok()
                    };
                    if let Some(kind) = body.read_error {
                        return Err(io::Error::new(kind, "reading JSON string syntax"));
                    }
                    valid
                };
                if !valid {
                    self.invalid_string = true;
                    return Err(invalid("invalid JSON string syntax"));
                }
                self.quotes_left = 2;
                return self.read(output);
            }
        }
        let bytes = self.cursor.fill_buf()?;
        let n = output.len().min(self.plain_left).min(bytes.len());
        output[..n].copy_from_slice(&bytes[..n]);
        self.cursor.consume(n);
        self.plain_left -= n;
        Ok(n)
    }
}

impl RangeSource {
    /// Validate grammar and whole-range UTF-8 once. Export archives use this boundary before
    /// validating individual conversations with Value's independently reset recursion budget.
    pub fn syntax(&self, range: Range) -> Result<bool> {
        self.syntax_with_strings(range, false)
    }
    fn syntax_with_strings(&self, range: Range, strict_strings: bool) -> Result<bool> {
        use serde::Deserialize;
        self.check_identity()?;
        let mut reader = SyntaxReader::new(self, range)?;
        reader.strict_strings = strict_strings;
        let result = {
            let mut decoder = serde_json::Deserializer::from_reader(&mut reader);
            serde::de::IgnoredAny::deserialize(&mut decoder).and_then(|_| decoder.end())
        };
        if let Err(error) = result {
            self.check_identity()?;
            if reader.invalid_string {
                return Ok(false);
            }
            if error.is_io() {
                return Err(error.into());
            }
            return Ok(false);
        }
        let valid = super::super::jsonl::ranges::valid_utf8(&mut self.cursor(range)?)?;
        self.check_identity()?;
        Ok(valid)
    }

    /// Scalar/depth validation after the syntax boundary. Short strings/numbers reuse the same
    /// authoritative checks as mapped slices; long strings reuse the bounded serde decoder.
    pub fn accepts_value(&self, range: Range) -> Result<bool> {
        self.accepts_value_at_depth(range, 0)
    }
    pub fn accepts_value_at_depth(&self, range: Range, depth: usize) -> Result<bool> {
        self.accepts_value_parts(range, depth, false)
    }
    fn accepts_value_parts(&self, range: Range, mut depth: usize, strings_accepted: bool) -> Result<bool> {
        let mut cursor = self.cursor(range)?;
        loop {
            cursor.whitespace()?;
            match cursor.peek()? {
                None => break,
                Some(b'"') => {
                    let string = cursor.string()?;
                    if strings_accepted {
                        continue;
                    }
                    let valid = if let Some(bytes) = cursor.borrow_range(string) {
                        super::super::json::value_compatible(bytes)
                    } else {
                        let body = Range {
                            offset: string.offset + 1,
                            len: string.len - 2,
                        };
                        let mut body = self.cursor(body)?;
                        let valid = super::super::json::validate_body(&mut body, true).is_ok();
                        if let Some(kind) = body.read_error {
                            return Err(io::Error::new(kind, "reading JSON string").into());
                        }
                        valid
                    };
                    if !valid {
                        return Ok(false);
                    }
                }
                Some(b'{' | b'[') => {
                    depth += 1;
                    if depth >= 128 {
                        return Ok(false);
                    }
                    cursor.consume(1);
                }
                Some(b'}' | b']') => {
                    depth = depth.checked_sub(1).context("unvalidated JSON nesting")?;
                    cursor.consume(1);
                }
                Some(b'-' | b'0'..=b'9') => {
                    let number = cursor.value()?;
                    let valid = if let Some(bytes) = cursor.borrow_range(number) {
                        super::super::json::value_compatible(bytes)
                    } else {
                        match serde_json::from_reader::<_, serde_json::Number>(self.cursor(number)?) {
                            Ok(_) => true,
                            Err(e) if e.is_io() => return Err(e.into()),
                            Err(_) => false,
                        }
                    };
                    if !valid {
                        return Ok(false);
                    }
                }
                Some(_) => cursor.consume(1),
            }
        }
        Ok(true)
    }
    pub fn validate(&self, range: Range) -> Result<bool> {
        let result = self.syntax_with_strings(range, true).and_then(|syntax| {
            if syntax {
                self.accepts_value_parts(range, 0, true)
            } else {
                Ok(false)
            }
        });
        self.check_identity()?;
        result
    }
    /// Explicit materialization is for demanded typed metadata/output, not structural traversal.
    pub fn materialize<T: serde::de::DeserializeOwned>(&self, range: Range) -> Result<T> {
        Ok(serde_json::from_reader(self.cursor(range)?)?)
    }
    pub fn for_each_string_chunk(&self, string: Range, mut visit: impl FnMut(&str)) -> Result<()> {
        if string.len <= crate::lazy::INLINE_MAX as u64 + 2 {
            let decoded: String = self.materialize(string)?;
            visit(&decoded);
            return Ok(());
        }
        anyhow::ensure!(string.len >= 2, "not a JSON string");
        let body = Range {
            offset: string.offset + 1,
            len: string.len - 2,
        };
        let mut cursor = self.cursor(body)?;
        let decoded = super::super::json::decode_body(&mut cursor, &mut |text| {
            visit(text);
            true
        });
        if let Some(kind) = cursor.read_error {
            return Err(io::Error::new(kind, "reading JSON string").into());
        }
        decoded.map_err(|_| anyhow::anyhow!("invalid JSON string"))?;
        Ok(())
    }
    /// Inspect the first byte of a normalized validated range without allocating an input window.
    pub fn kind(&self, range: Range) -> Result<Option<u8>> {
        if range.len == 0 {
            return Ok(None);
        }
        anyhow::ensure!(range.end()? <= self.range().end()?, "JSON range outside source");
        let mut byte = [0];
        loop {
            match if let Some(memory) = &self.memory {
                let source = memory.memory_bytes().unwrap();
                if let Some(b) = source.get(range.offset as usize) {
                    byte[0] = *b;
                    Ok(1)
                } else {
                    Ok(0)
                }
            } else {
                read_at(self.file.as_ref().unwrap(), &mut byte, range.offset)
            } {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "JSON source was truncated").into()),
                Ok(_) => return Ok(Some(byte[0])),
            }
        }
    }
    /// Stream a string accepted at the enclosing validation boundary, stopping on callback error.
    /// The enclosing operation checks source identity even after an intentional prefix stop.
    pub fn for_each_string_chunk_until(&self, string: Range, visit: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        if string.len <= crate::lazy::INLINE_MAX as u64 + 2 {
            let decoded: String = self.materialize(string)?;
            return visit(&decoded);
        }
        anyhow::ensure!(string.len >= 2, "not a JSON string");
        let body = Range {
            offset: string.offset + 1,
            len: string.len - 2,
        };
        let mut cursor = self.cursor(body)?;
        let mut callback_error = None;
        let decoded = super::super::json::decode_body(&mut cursor, &mut |part| {
            callback_error = visit(part).err();
            callback_error.is_none()
        });
        if let Some(kind) = cursor.read_error {
            return Err(io::Error::new(kind, "reading JSON string").into());
        }
        if let Some(error) = callback_error {
            return Err(error);
        }
        decoded
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("validated JSON string failed decoding"))
    }
    pub fn key_matches(&self, string: Range, key: &str) -> Result<bool> {
        // An escaped ASCII byte needs at most six encoded bytes; surrogate pairs encode one
        // four-byte UTF-8 scalar in twelve bytes. A longer key cannot equal the requested name.
        if string.len > (key.len() as u64).saturating_mul(6).saturating_add(2) {
            return Ok(false);
        }
        let mut matched = true;
        let mut position: usize = 0;
        self.for_each_string_chunk(string, |part| {
            matched &= key.as_bytes().get(position..position.saturating_add(part.len())) == Some(part.as_bytes());
            position = position.saturating_add(part.len());
        })?;
        Ok(matched && position == key.len())
    }
    pub fn array(&self, range: Range) -> Result<Option<ArrayRanges<'_>>> {
        let mut cursor = self.cursor(range)?;
        cursor.whitespace()?;
        if cursor.peek()? != Some(b'[') {
            return Ok(None);
        }
        cursor.consume(1);
        Ok(Some(ArrayRanges { cursor, done: false }))
    }
    pub fn object(&self, range: Range) -> Result<Option<ObjectRanges<'_>>> {
        let mut cursor = self.cursor(range)?;
        cursor.whitespace()?;
        if cursor.peek()? != Some(b'{') {
            return Ok(None);
        }
        cursor.consume(1);
        Ok(Some(ObjectRanges { cursor, done: false }))
    }
    /// Retain only the requested roster. Arbitrary ignored keys cannot consume decoded heap.
    pub fn select_fields(&self, range: Range, keys: &[&str]) -> Result<Option<Vec<Option<Range>>>> {
        self.select_fields_with(range, keys, false)
    }
    pub fn select_unique_fields(&self, range: Range, keys: &[&str]) -> Result<Option<Vec<Option<Range>>>> {
        self.select_fields_with(range, keys, true)
    }
    fn select_fields_with(&self, range: Range, keys: &[&str], unique: bool) -> Result<Option<Vec<Option<Range>>>> {
        let Some(entries) = self.object(range)? else {
            return Ok(None);
        };
        let max_key = keys.iter().map(|key| key.len()).max().unwrap_or(0) as u64;
        let mut fields = vec![None; keys.len()];
        for entry in entries {
            let (key, value) = entry?;
            if unique && !self.accepts_value(key)? {
                return Err(super::InvalidStruct("invalid object key").into());
            }
            if key.len > max_key.saturating_mul(6).saturating_add(2) {
                continue;
            }
            let key: String = self.materialize(key)?;
            for (i, requested) in keys.iter().enumerate() {
                if key == *requested {
                    if unique && fields[i].is_some() {
                        return Err(super::InvalidStruct("duplicate known field").into());
                    }
                    fields[i] = Some(value);
                }
            }
        }
        Ok(Some(fields))
    }
    pub fn text(&self, string: Range, spans: bool) -> Result<Option<crate::lazy::Text>> {
        if self.kind(string)? != Some(b'"') {
            return Ok(None);
        }
        if !spans || string.len - 2 <= crate::lazy::INLINE_MAX as u64 {
            return Ok(Some(crate::lazy::Text::Inline(self.materialize(string)?)));
        }
        let body = Range {
            offset: string.offset + 1,
            len: string.len - 2,
        };
        let mut cursor = self.cursor(body)?;
        let escaped = loop {
            let bytes = cursor.fill_buf()?;
            if bytes.is_empty() {
                break false;
            }
            if memchr::memchr(b'\\', bytes).is_some() {
                break true;
            }
            let n = bytes.len();
            cursor.consume(n);
        };
        Ok(Some(crate::lazy::Text::Span(crate::lazy::Span {
            source: None,
            offset: body.offset,
            len: body.len,
            escaped,
        })))
    }
    /// Scan with constant state and final-key-wins semantics, including discarded duplicate keys.
    pub fn get(&self, range: Range, key: &str) -> Result<Option<Range>> {
        let Some(fields) = self.object(range)? else {
            return Ok(None);
        };
        let mut found = None;
        for field in fields {
            let (name, value) = field?;
            if self.key_matches(name, key)? {
                found = Some(value);
            }
        }
        Ok(found)
    }
}

pub(crate) struct ArrayRanges<'a> {
    cursor: Cursor<'a>,
    done: bool,
}
impl Iterator for ArrayRanges<'_> {
    type Item = io::Result<Range>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (|| {
            self.cursor.whitespace()?;
            if self.cursor.peek()? == Some(b']') {
                self.done = true;
                return Ok(None);
            }
            let value = self.cursor.value()?;
            self.cursor.whitespace()?;
            match self.cursor.peek()? {
                Some(b',') => self.cursor.consume(1),
                Some(b']') => self.done = true,
                _ => return Err(invalid("unvalidated JSON array delimiter")),
            }
            Ok(Some(value))
        })();
        match result {
            Ok(value) => value.map(Ok),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
pub(crate) struct ObjectRanges<'a> {
    cursor: Cursor<'a>,
    done: bool,
}
impl Iterator for ObjectRanges<'_> {
    type Item = io::Result<(Range, Range)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (|| {
            self.cursor.whitespace()?;
            if self.cursor.peek()? == Some(b'}') {
                self.done = true;
                return Ok(None);
            }
            let key = self.cursor.string()?;
            self.cursor.whitespace()?;
            if self.cursor.peek()? != Some(b':') {
                return Err(invalid("unvalidated JSON field delimiter"));
            }
            self.cursor.consume(1);
            let value = self.cursor.value()?;
            self.cursor.whitespace()?;
            match self.cursor.peek()? {
                Some(b',') => self.cursor.consume(1),
                Some(b'}') => self.done = true,
                _ => return Err(invalid("unvalidated JSON object delimiter")),
            }
            Ok(Some((key, value)))
        })();
        match result {
            Ok(value) => value.map(Ok),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod acceptance_tests {
    use super::*;

    #[test]
    fn bounded_validation_matches_value_and_navigation_keeps_final_escaped_keys() {
        let path = std::env::temp_dir().join(format!("cv-range-validation-{}", uuid::Uuid::new_v4()));
        let mut cases = [
            "null",
            "true",
            "0",
            "-0",
            "1e400",
            "1e-400",
            "18446744073709551616",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"\\ud83c\\udf19\"",
            "\"\\\\ud800\"",
            "{\"x\":1e400,\"x\":0}",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
        }
        cases.extend([
            format!("\"{}\\ud800\"", "\\u0061".repeat(WINDOW)),
            format!("\"{}é😀\"", "a".repeat(WINDOW - 2)),
            format!("1.{}9", "0".repeat(WINDOW * 2)),
            format!("1e-{}", "9".repeat(WINDOW * 2)),
        ]);
        for case in cases {
            std::fs::write(&path, &case).unwrap();
            let source = RangeSource::open(&path).unwrap();
            assert_eq!(
                source.syntax(source.range()).unwrap() && source.accepts_value(source.range()).unwrap(),
                serde_json::from_str::<serde_json::Value>(&case).is_ok(),
                "separate acceptance: {} bytes",
                case.len()
            );
            assert_eq!(
                source.validate(source.range()).unwrap(),
                serde_json::from_str::<serde_json::Value>(&case).is_ok(),
                "{} bytes",
                case.len()
            );
        }
        let document = r#" {"z":1,"a":[null,{"x":"tail"}],"\u007a":2} "#;
        std::fs::write(&path, document).unwrap();
        let source = RangeSource::open(&path).unwrap();
        assert!(source.validate(source.range()).unwrap());
        let z = source.get(source.range(), "z").unwrap().unwrap();
        assert_eq!(
            source.materialize::<serde_json::Value>(z).unwrap(),
            serde_json::json!(2)
        );
        assert!(source.get(source.range(), "missing").unwrap().is_none());
        let a = source.get(source.range(), "a").unwrap().unwrap();
        let values = source
            .array(a)
            .unwrap()
            .unwrap()
            .map(|range| source.materialize::<serde_json::Value>(range.unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, [serde_json::Value::Null, serde_json::json!({"x":"tail"})]);
        std::fs::write(&path, b"{\"ignored\":\"\xff\"}").unwrap();
        let invalid = RangeSource::open(&path).unwrap();
        assert!(!invalid.syntax(invalid.range()).unwrap());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;
    #[test]
    fn cached_endpoint_never_extends_a_requested_range() {
        let path = std::env::temp_dir().join(format!("cv-range-cache-{}", uuid::Uuid::new_v4()));
        let text = format!("  \"{}\"  ", "\\n\\\"".repeat(WINDOW));
        std::fs::write(&path, &text).unwrap();
        let source = RangeSource::open(&path).unwrap();
        assert!(source.validate(source.range()).unwrap());
        let expected = source.cursor(source.range()).unwrap().value().unwrap();
        assert!(source.index.get(expected.offset as usize).is_some());
        assert_eq!(source.cursor(source.range()).unwrap().value().unwrap(), expected);
        let bounded = Range {
            offset: expected.offset,
            len: expected.len - 1,
        };
        assert!(source.cursor(bounded).unwrap().string().is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_late_refill_failure_is_a_read_error_not_an_invalid_scalar_or_success() {
        let path = std::env::temp_dir().join(format!("cv-range-refill-{}", uuid::Uuid::new_v4()));
        let encoded = serde_json::to_string(&"x".repeat(WINDOW * 3)).unwrap();
        std::fs::write(&path, &encoded).unwrap();
        let source = RangeSource::open(&path).unwrap();
        assert!(source.validate(source.range()).unwrap());
        let mut callbacks = 0;
        let result = source.for_each_string_chunk(source.range(), |_| {
            callbacks += 1;
            if callbacks == 1 {
                std::fs::write(&path, "x").unwrap();
            }
        });
        assert!(callbacks > 0);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(source.check_identity().is_err());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_roster_and_lazy_text_keep_original_ranges() {
        let body = "雪\\".repeat(4000);
        let encoded = format!(
            "{{\"id\":1,\"\\u0069d\":2,\"ignored{}\":null,\"body\":{}}}",
            "x".repeat(20000),
            serde_json::to_string(&body).unwrap()
        );
        let path = std::env::temp_dir().join(format!("cv-range-fields-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, &encoded).unwrap();
        let source = RangeSource::open(&path).unwrap();
        assert!(source.validate(source.range()).unwrap());
        let fields = source
            .select_fields(source.range(), &["id", "id", "missing", "body"])
            .unwrap()
            .unwrap();
        assert!(source.select_unique_fields(source.range(), &["id", "body"]).is_err());
        assert!(source
            .select_unique_fields(source.range(), &["body"])
            .unwrap()
            .is_some());
        assert_eq!(fields[0], fields[1]);
        assert!(fields[2].is_none());
        assert_eq!(source.materialize::<u64>(fields[0].unwrap()).unwrap(), 2);
        let crate::lazy::Text::Span(span) = source.text(fields[3].unwrap(), true).unwrap().unwrap() else {
            panic!("large string must remain a span")
        };
        assert!(span.escaped);
        assert_eq!(
            crate::lazy::Resolver::new(Some(path.clone()))
                .try_resolve(&span)
                .unwrap(),
            body
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn quote_search_preserves_slash_parity_across_windows() {
        let path = std::env::temp_dir().join(format!("cv-range-parity-{}", uuid::Uuid::new_v4()));
        for body in [
            r"\u0061".repeat(WINDOW),
            r"\\".repeat(WINDOW + 1),
            "a".repeat(WINDOW - 2) + r#"\"after"#,
            "a".repeat(WINDOW - 3) + r#"\\"#,
            "a".repeat(WINDOW - 1) + &r"\\".repeat(WINDOW) + r#"\"after"#,
        ] {
            let encoded = format!("\"{body}\" null");
            let string_len = encoded.len() - 5;
            std::fs::write(&path, &encoded).unwrap();
            let source = RangeSource::open(&path).unwrap();
            let range = source.cursor(source.range()).unwrap().string().unwrap();
            assert_eq!(
                range,
                Range {
                    offset: 0,
                    len: string_len as u64
                }
            );
            assert!(serde_json::from_str::<String>(&encoded[..string_len]).is_ok());
            let mut bytes = Cursor::bytes(encoded.as_bytes(), 0).unwrap();
            assert_eq!(bytes.string().unwrap(), range);
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn byte_and_positioned_cursors_agree_across_escaped_window_boundaries() {
        let path = std::env::temp_dir().join(format!("cv-range-cursor-{}", uuid::Uuid::new_v4()));
        let values = [
            serde_json::json!("a".repeat(WINDOW - 2) + "\\\"é😀"),
            serde_json::json!({"x":[1,true,null,"end"]}),
            serde_json::json!(-1.25e30),
        ];
        let encoded = values
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join(" \n");
        std::fs::write(&path, &encoded).unwrap();
        let source = RangeSource::open(&path).unwrap();
        let mut file = source.cursor(source.range()).unwrap();
        let mut bytes = Cursor::bytes(encoded.as_bytes(), 0).unwrap();
        for expected in values {
            let fr = file.value().unwrap();
            let br = bytes.value().unwrap();
            assert_eq!(fr, br);
            assert_eq!(file.position(), bytes.position());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(bytes.borrow_range(br).unwrap()).unwrap(),
                expected
            );
            let mut scalar = source.cursor(fr).unwrap();
            assert_eq!(
                serde_json::from_reader::<_, serde_json::Value>(&mut scalar).unwrap(),
                expected
            );
        }
        file.whitespace().unwrap();
        assert_eq!(file.peek().unwrap(), None);
        assert_eq!(source.kind(source.range()).unwrap(), Some(b'"'));
        let first = source.cursor(source.range()).unwrap().value().unwrap();
        let mut chunks = 0;
        let error = source
            .for_each_string_chunk_until(first, &mut |_| {
                chunks += 1;
                anyhow::bail!("stop after first bounded chunk")
            })
            .unwrap_err();
        assert_eq!(chunks, 1);
        assert!(error.to_string().contains("stop after first"));
        source.check_identity().unwrap();
        std::fs::write(&path, "truncated").unwrap();
        assert!(source.check_identity().is_err());
        let mut truncated = source.cursor(source.range()).unwrap();
        assert!(truncated.read_to_end(&mut Vec::new()).is_err());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod syntax_tests {
    use super::*;
    use serde::Deserialize;

    fn oracle(bytes: &[u8]) -> bool {
        let mut parser = serde_json::Deserializer::from_slice(bytes);
        serde::de::IgnoredAny::deserialize(&mut parser)
            .and_then(|_| parser.end())
            .is_ok()
            && std::str::from_utf8(bytes).is_ok()
    }

    #[test]
    fn grammar_reader_preserves_a_late_io_failure() {
        let path = std::env::temp_dir().join(format!("cv-syntax-io-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("[null,\"{}\"]", "a".repeat(WINDOW * 3))).unwrap();
        let source = RangeSource::open(&path).unwrap();
        let mut reader = SyntaxReader::new(&source, source.range()).unwrap();
        assert_eq!(reader.read(&mut [0; 6]).unwrap(), 6);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1)
            .unwrap();
        let mut parser = serde_json::Deserializer::from_reader(&mut reader);
        let error = serde::de::IgnoredAny::deserialize(&mut parser).unwrap_err();
        assert!(error.is_io());
        assert!(!reader.invalid_string);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bounded_string_grammar_matches_ignored_any_without_value_narrowing() {
        let path = std::env::temp_dir().join(format!("cv-range-syntax-{}", uuid::Uuid::new_v4()));
        let mut cases = vec![
            br#"{"a":"x","b":[true,null,-1.2e3],"a":2}"#.to_vec(),
            br#"{"\ud800":"\udc00","a":1e9999}"#.to_vec(),
            br#"["\ud800x","\udc00","\ud800\u0041"]"#.to_vec(),
            br#"{"x" "y"}"#.to_vec(),
            br#"["a""b"]"#.to_vec(),
            br#"[true,]"#.to_vec(),
            br#"{1:"x"}"#.to_vec(),
            br#""abc\q""#.to_vec(),
            br#""abc\u00xz""#.to_vec(),
            b"\"invalid\xff\"".to_vec(),
            b"\"raw\ncontrol\"".to_vec(),
            b"\"unterminated".to_vec(),
        ];
        for delta in 0..16 {
            let pad = "a".repeat(WINDOW - 16 + delta);
            for tail in [
                r"\ud800",
                r"\udc00",
                r"\ud800\udc00",
                r"\ud800x",
                r#"\\\""#,
                "é😀",
                r"\u000x",
                "\\",
            ] {
                cases.push(format!("[\"{pad}{tail}\",null]").into_bytes());
            }
        }
        for token in [b"null".as_slice(), b"true", b"1.0e10", br#""\ud800""#, br#"{"a":[1]}"#] {
            for end in 0..=token.len() {
                cases.push(token[..end].to_vec());
            }
        }
        for bytes in cases {
            std::fs::write(&path, &bytes).unwrap();
            let source = RangeSource::open(&path).unwrap();
            assert_eq!(
                source.syntax(source.range()).unwrap(),
                oracle(&bytes),
                "{} bytes: {:?}",
                bytes.len(),
                &bytes[bytes.len().saturating_sub(32)..]
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
