//! Lazy content: the IR holds large text as a **span** into its source file, not as owned bytes.
//!
//! A fully-parsed [`Session`](crate::ir::Session) used to *own* every byte of content
//! (`Block::Text { text: String }`, …) — 700 MB resident just to hold the IR of a transcript with
//! one 700 MB record, even if the consumer only wants its title. There's no need: the bytes live on
//! disk in a read-only transcript. So a content field is a [`Text`] that's either materialized
//! [`Inline`](Text::Inline) (small — most messages) or a [`Span`] (large), resolved **on demand**
//! against the source via a [`Resolver`] (which memory-maps the file, so even resolving touches only
//! the needed pages).
//!
//! Invariant: reading a `Span` as `&str` (via [`Deref`]) panics — you must [`resolve`](Text::resolve)
//! it against a [`Resolver`], or [`materialize`](crate::ir::Session::materialize) the session first.
//! Streaming consumers resolve per-block (peak = one field); whole-session consumers materialize
//! (peak = whole session, their choice). Below the inline threshold everything is `Inline`, where
//! `Deref`/`Display` just work.

mod source;
pub use source::SourceText;

use crate::harness::store::json::{complete_escape_prefix, decode_body, unescape_body, validate_body};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Read at most `max_bytes` from the head of a sidecar text file (a persisted tool output, say) as
/// lossy UTF-8. `None` if the file can't be opened. Indexers use this to make text the model never
/// saw inline (only via a stub) searchable, without ever loading a whole multi-MB dump.
pub fn read_head(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(max_bytes.min(1 << 20));
    f.by_ref().take(max_bytes as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A persisted output's head as one demand found it.
#[derive(Debug, PartialEq, Eq)]
pub enum PersistedHead {
    /// No file at the path: the output was never kept, or was cleaned up.
    Absent,
    Text(String),
    /// The file exists but cannot be read here (permissions, not a regular file): a session that
    /// references it degrades to its transcript, the reason noted, rather than failing.
    Unreadable(String),
}

impl PersistedHead {
    /// The text a reader indexes for this output: its head, a note naming why it is missing, or
    /// nothing when it is absent.
    pub fn indexed_text(&self, path: &str) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Text(text) => Some(text.clone()),
            Self::Unreadable(reason) => Some(format!("[persisted output {path} unreadable: {reason}]")),
        }
    }
}

/// Read an optional persisted output head. Absence and a file that cannot be opened are answers;
/// a change while it is read is an error. Identity is captured for this demand, without a
/// historical source stamp.
pub fn read_head_checked(path: &Path, max_bytes: usize) -> anyhow::Result<PersistedHead> {
    use std::io::Read;
    let path = std::path::absolute(path)?;
    let unreadable = |error: &dyn std::fmt::Display| PersistedHead::Unreadable(format!("{error:#}"));
    let captured = match crate::json::JsonSource::capture(&path) {
        Ok(captured) => captured,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(PersistedHead::Absent)
        }
        Err(error) => return Ok(unreadable(&error)),
    };
    // Absence is optional only at initial capture. Disappearance while opening/reading a source
    // already observed present is a failed demand, like any other mid-demand mutation.
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Ok(unreadable(&error)),
        Err(error) => return Err(error.into()),
    };
    let source = CheckedSource::opened(path, captured, file)?;
    let len = usize::try_from(source.len.min(u64::try_from(max_bytes).unwrap_or(u64::MAX)))?;
    let mut bytes = vec![0; len];
    let result = source.file.try_clone()?.read_exact(&mut bytes);
    source.check()?;
    result?;
    Ok(PersistedHead::Text(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Content at or below this many bytes is stored inline; larger content becomes a lazy [`Span`].
/// Small on purpose: most messages are well under it, so typical sessions are untouched and only
/// genuinely large fields (file dumps, pasted blobs, base64) go lazy.
pub const INLINE_MAX: usize = 4096;

/// A span of bytes in a source file, resolved on demand. `offset`/`len` cover the **raw** bytes; if
/// the source is a JSON string body, `escaped` marks that the slice needs JSON-unescaping on resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Source file. `None` ⇒ the owning [`Session`](crate::ir::Session)'s `source_path` (the common
    /// case); set only for adapters whose content lives in a sidecar (codex/grok update streams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    pub offset: u64,
    pub len: u64,
    /// The raw slice is a JSON string body needing unescaping (`true`) or literal text (`false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escaped: bool,
}

/// A piece of content text: materialized inline (small) or a lazy [`Span`] (large). See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    Inline(String),
    Span(Span),
    Source(SourceText),
}

impl Text {
    /// The inline string, or `None` if this is an unresolved span.
    pub fn inline_str(&self) -> Option<&str> {
        match self {
            Text::Inline(s) => Some(s),
            Text::Span(_) | Text::Source(_) => None,
        }
    }

    pub fn is_span(&self) -> bool {
        matches!(self, Text::Span(_) | Text::Source(_))
    }

    /// The span, if this is lazy content.
    pub fn as_span(&self) -> Option<&Span> {
        match self {
            Text::Span(sp) => Some(sp),
            Text::Inline(_) | Text::Source(_) => None,
        }
    }

    /// Resolve demanded text, reporting unavailable, changed or unreadable sources.
    pub fn try_resolve<'a>(&'a self, resolver: &'a Resolver) -> anyhow::Result<Cow<'a, str>> {
        match self {
            Text::Inline(s) => Ok(Cow::Borrowed(s)),
            Text::Span(sp) => resolver.try_resolve(sp),
            Text::Source(source) => Ok(Cow::Owned(source.try_resolve()?)),
        }
    }

    /// Checked counterpart of `resolve_prefix`, with the same raw-byte selection policy.
    pub fn try_resolve_prefix<'a>(&'a self, resolver: &'a Resolver, max_raw: u64) -> anyhow::Result<Cow<'a, str>> {
        match self {
            Text::Inline(s) => Ok(Cow::Borrowed(s)),
            Text::Span(sp) => resolver.try_resolve_prefix(sp, max_raw),
            Text::Source(source) => Ok(Cow::Owned(
                source.try_prefix(usize::try_from(max_raw).unwrap_or(usize::MAX))?,
            )),
        }
    }

    /// Length in bytes without resolving (a span's raw length — an upper bound on the unescaped len).
    pub fn len(&self) -> usize {
        match self {
            Text::Inline(s) => s.len(),
            Text::Span(sp) => usize::try_from(sp.len).unwrap_or(usize::MAX),
            Text::Source(source) => source.raw_len(),
        }
    }

    pub fn try_for_each_chunk(
        &self,
        resolver: &Resolver,
        chunk: usize,
        mut f: impl FnMut(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        match self {
            Text::Inline(text) => {
                let mut part = text.as_str();
                while !part.is_empty() {
                    let mut end = part.len().min(chunk.max(1));
                    while !part.is_char_boundary(end) {
                        end += 1;
                    }
                    f(&part[..end])?;
                    part = &part[end..];
                }
                Ok(())
            }
            Text::Span(span) => resolver.try_for_each_chunk(span, chunk, f),
            Text::Source(source) => source.try_for_each_chunk(chunk, f),
        }
    }
    pub fn try_decoded_len(&self, resolver: &Resolver) -> anyhow::Result<usize> {
        match self {
            Text::Inline(text) => Ok(text.len()),
            Text::Source(source) => source.try_decoded_len(),
            Text::Span(span) => resolver.try_resolved_len(span),
        }
    }
    pub fn with_skipped_prefix(mut self, bytes: usize) -> anyhow::Result<Self> {
        match &mut self {
            Text::Inline(text) => {
                *text = text
                    .get(bytes..)
                    .ok_or_else(|| anyhow::anyhow!("text prefix past end or invalid UTF8 boundary"))?
                    .to_owned();
            }
            Text::Source(source) => {
                let prefix = source.try_prefix(bytes.saturating_add(3))?;
                anyhow::ensure!(
                    prefix.get(bytes..).is_some(),
                    "source text prefix past end or invalid UTF8 boundary"
                );
                source.skip = source
                    .skip
                    .checked_add(bytes)
                    .ok_or_else(|| anyhow::anyhow!("text prefix overflow"))?;
            }
            Text::Span(span) if !span.escaped && bytes as u64 <= span.len => {
                span.offset = span
                    .offset
                    .checked_add(bytes as u64)
                    .ok_or_else(|| anyhow::anyhow!("text prefix offset overflow"))?;
                span.len -= bytes as u64;
            }
            Text::Span(_) => anyhow::bail!("decoded prefix needs source_text for escaped spans"),
        }
        Ok(self)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Text {
    fn default() -> Self {
        Text::Inline(String::new())
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Text::Inline(s)
    }
}
impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Text::Inline(s.to_string())
    }
}

/// Read ergonomics: an inline `Text` derefs to its `str`. **Panics on an unresolved `Span`** — the
/// caller must `resolve`/`materialize` first. Inline content (the common case) is unaffected.
impl Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Text::Inline(s) => s,
            Text::Span(_) | Text::Source(_) => {
                panic!("Text::Span read as &str without resolving — call Session::materialize() or Text::resolve(&resolver) first")
            }
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Text::Inline(s) => f.write_str(s),
            // Don't panic in Display; an unmaterialized span renders empty. Materialize for output.
            Text::Span(_) | Text::Source(_) => Ok(()),
        }
    }
}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        self.inline_str() == Some(other)
    }
}
impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        self.inline_str() == Some(*other)
    }
}

/// Serializes as a plain JSON string when inline (so a materialized `Session` round-trips
/// byte-identically), or as the span object when not. Whole-session JSON paths
/// [`materialize`](crate::ir::Session::materialize) first, so spans don't reach serialization there.
impl Serialize for Text {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Text::Inline(t) => s.serialize_str(t),
            Text::Span(sp) => sp.serialize(s),
            Text::Source(source) => s.serialize_str(&source.try_resolve().map_err(serde::ser::Error::custom)?),
        }
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string (inline text) or a span object")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Text, E> {
                Ok(Text::Inline(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Text, E> {
                Ok(Text::Inline(v))
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Text, A::Error> {
                let sp = Span::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(Text::Span(sp))
            }
        }
        d.deserialize_any(V)
    }
}

/// The identity a session's primary source had when its spans were parsed (or, for a resolver
/// built before the parse, when the resolver was built). Spans carry offsets, not identities; this
/// is what lets a resolver tell a file that only grew from one that was replaced.
#[derive(Clone, Debug)]
pub struct SourceStamp(crate::json::JsonSource);
impl SourceStamp {
    /// `None` when `path` cannot be stat'ed now; resolution then reports that failure itself.
    pub fn capture(path: &Path) -> Option<Self> {
        crate::json::JsonSource::capture_identity(path).ok().map(Self)
    }
    /// Whether this stamps `path` (spelled however the caller spells it).
    pub fn is_for(&self, path: &Path) -> bool {
        std::path::absolute(path).is_ok_and(|path| path == self.0.path())
    }
}

/// Resolves [`Span`]s against their source file(s). With the `mmap` feature each demand maps only
/// its span's bytes and drops them when it returns, so resolving every span of a multi-GB
/// transcript never holds the file mapped; without `mmap` (e.g. wasm32) it reads positioned.
///
/// The primary source is held to its [`SourceStamp`]: a demand fails (checked) or resolves empty
/// (unchecked) unless the file is the stamped one, unchanged or only appended to. That catches a
/// file replaced under its name (a new inode) or rewritten at the same length after the parse, on
/// the first demand. An in-place rewrite that also grows the file keeps the inode and passes, the
/// same limit resume marks document.
pub struct Resolver {
    primary: Option<PathBuf>,
    stamp: Option<SourceStamp>,
    #[cfg(feature = "mmap")]
    checked_files: std::cell::RefCell<std::collections::HashMap<PathBuf, FirstDemand>>,
}

/// A capture at demand time; [`Resolver::admit`] holds it to the parse-time stamp.
struct CheckedSource {
    captured: crate::json::JsonSource,
    path: PathBuf,
    file: std::fs::File,
    len: u64,
}
impl CheckedSource {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let path = std::path::absolute(path)?;
        let captured = crate::json::JsonSource::capture(&path)?;
        Self::open_captured(path, captured)
    }
    fn open_captured(path: PathBuf, captured: crate::json::JsonSource) -> anyhow::Result<Self> {
        use anyhow::Context;
        let file = std::fs::File::open(&path).with_context(|| format!("opening Text source {}", path.display()))?;
        Self::opened(path, captured, file)
    }
    fn opened(path: PathBuf, captured: crate::json::JsonSource, file: std::fs::File) -> anyhow::Result<Self> {
        let metadata = file.metadata()?;
        let len = metadata.len();
        captured.check_metadata(metadata)?;
        captured.check()?;
        Ok(Self {
            captured,
            path,
            file,
            len,
        })
    }
    fn check(&self) -> anyhow::Result<()> {
        self.captured.check_metadata(self.file.metadata()?)?;
        self.captured.check()
    }
    fn bounds(&self, span: &Span) -> anyhow::Result<()> {
        anyhow::ensure!(
            span.offset.checked_add(span.len).is_some_and(|end| end <= self.len),
            "Text span is outside source {}",
            self.path.display()
        );
        Ok(())
    }
    #[cfg(not(feature = "mmap"))]
    fn reader(&self, offset: u64) -> anyhow::Result<std::io::BufReader<std::fs::File>> {
        use std::io::Seek;
        let mut reader = std::io::BufReader::with_capacity(64 * 1024, self.file.try_clone()?);
        reader.seek(std::io::SeekFrom::Start(offset))?;
        Ok(reader)
    }
}

/// The file a resolver's first checked demand on a path saw; later demands must see it too.
#[cfg(feature = "mmap")]
struct FirstDemand {
    captured: crate::json::JsonSource,
    file: std::fs::File,
}

/// One demand's span bytes, mapped for that demand only.
#[cfg(feature = "mmap")]
enum SpanBytes {
    Empty,
    Mapped(memmap2::Mmap),
}
#[cfg(feature = "mmap")]
impl AsRef<[u8]> for SpanBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Mapped(map) => map,
        }
    }
}

#[derive(Default)]
struct DemandState {
    error: std::cell::RefCell<Option<anyhow::Error>>,
}
impl DemandState {
    fn fail(&self, error: anyhow::Error) {
        let mut first = self.error.borrow_mut();
        if first.is_none() {
            *first = Some(error);
        }
    }
    fn failed(&self) -> bool {
        self.error.borrow().is_some()
    }
    fn finish(&self, source: &CheckedSource) -> anyhow::Result<()> {
        // A callback's early error must never hide source mutation.
        source.check()?;
        self.error.borrow_mut().take().map_or(Ok(()), Err)
    }
}

/// Existing lossy/JSON helpers distinguish malformed bytes from failure through this tracker.
/// Periodic checks bound unchecked source progress; callbacks check immediately on return.
struct DemandReader<'a, R> {
    inner: R,
    source: &'a CheckedSource,
    state: &'a DemandState,
    since_check: usize,
}
const DEMAND_WINDOW: usize = 64 * 1024;
impl<R> DemandReader<'_, R> {
    fn before_read(&mut self) -> std::io::Result<()> {
        if !self.state.failed() && self.since_check >= DEMAND_WINDOW {
            if let Err(error) = self.source.check() {
                self.state.fail(error);
            }
            self.since_check = 0;
        }
        if self.state.failed() {
            Err(std::io::Error::other("Text demand failed"))
        } else {
            Ok(())
        }
    }
    fn io_error(&self, error: std::io::Error) -> std::io::Error {
        let kind = error.kind();
        self.state.fail(error.into());
        std::io::Error::new(kind, "reading Text source")
    }
}
impl<R: std::io::BufRead> std::io::BufRead for DemandReader<'_, R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.before_read()?;
        match self.inner.fill_buf() {
            // Cursor-backed mmap readers otherwise expose the entire remaining file. Lossy
            // UTF8 validation would scan it before the first bounded callback could stop.
            Ok(bytes) => Ok(&bytes[..bytes.len().min(DEMAND_WINDOW)]),
            Err(error) => {
                let kind = error.kind();
                self.state.fail(error.into());
                Err(std::io::Error::new(kind, "reading Text source"))
            }
        }
    }
    fn consume(&mut self, n: usize) {
        self.since_check = self.since_check.saturating_add(n);
        self.inner.consume(n);
    }
}
impl<R: std::io::BufRead> std::io::Read for DemandReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead;
        let bytes = self.fill_buf()?;
        let n = bytes.len().min(output.len());
        output[..n].copy_from_slice(&bytes[..n]);
        self.consume(n);
        Ok(n)
    }
}
impl<R: std::io::Seek> std::io::Seek for DemandReader<'_, R> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.before_read()?;
        self.since_check = 0;
        self.inner.seek(position).map_err(|e| self.io_error(e))
    }
}

impl Resolver {
    /// A resolver rooted at a session's primary source (its `source_path`), stamped now. Build it
    /// before the parse whose spans it resolves; after a parse, use [`crate::Session::resolver`],
    /// which carries the parse-time stamp.
    pub fn new(primary: Option<PathBuf>) -> Self {
        let stamp = primary.as_deref().and_then(SourceStamp::capture);
        Self::stamped(primary, stamp)
    }

    /// A resolver whose primary is held to `stamp` (none: the primary is not held to anything).
    pub fn stamped(primary: Option<PathBuf>, stamp: Option<SourceStamp>) -> Self {
        Resolver {
            primary,
            stamp,
            #[cfg(feature = "mmap")]
            checked_files: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    fn path_for<'a>(&'a self, sp: &'a Span) -> Option<&'a Path> {
        sp.source.as_deref().or(self.primary.as_deref())
    }

    fn checked_source(&self, sp: &Span) -> anyhow::Result<CheckedSource> {
        use anyhow::Context;
        let source = CheckedSource::open(self.path_for(sp).context("Text span has no source path")?)?;
        if let Some(stamp) = self.stamp.as_ref().filter(|stamp| stamp.0.path() == source.path) {
            anyhow::ensure!(
                stamp.0.admits(&source.captured),
                "Text source {} was replaced or rewritten after it was parsed",
                source.path.display()
            );
        }
        source.bounds(sp)?;
        Ok(source)
    }

    #[cfg(feature = "mmap")]
    fn checked_bytes(&self, sp: &Span, source: &CheckedSource) -> anyhow::Result<SpanBytes> {
        if sp.len == 0 {
            return Ok(SpanBytes::Empty);
        }
        let mut first = self.checked_files.borrow_mut();
        if !first.contains_key(&source.path) {
            first.insert(
                source.path.clone(),
                FirstDemand {
                    captured: source.captured.clone(),
                    file: source.file.try_clone()?,
                },
            );
        }
        let first = &first[&source.path];
        // The first checked demand is remembered; later demands must see that same file.
        first.captured.check_metadata(source.file.metadata()?)?;
        source.captured.check_metadata(first.file.metadata()?)?;
        source.check()?;
        let len = usize::try_from(sp.len)?;
        // `checked_source` bounded the span by the captured length.
        // SAFETY: a read-only mapping of the demand's own descriptor, dropped with the demand.
        // File identity guards detect ordinary changes around each demand, not atomic snapshots.
        let map = unsafe {
            memmap2::MmapOptions::new()
                .offset(sp.offset)
                .len(len)
                .map(&source.file)?
        };
        #[cfg(test)]
        crate::harness::store::jsonl::note_mapped(len);
        Ok(SpanBytes::Mapped(map))
    }

    /// Checked text materialization. Inline/native unescaped text can borrow; escaped output
    /// retains the historical raw lossy fallback for malformed JSON escapes.
    pub fn try_resolve<'a>(&'a self, sp: &Span) -> anyhow::Result<Cow<'a, str>> {
        let source = self.checked_source(sp)?;
        let state = DemandState::default();
        #[cfg(feature = "mmap")]
        let output = {
            let held = self.checked_bytes(sp, &source)?;
            let bytes = held.as_ref();
            if !sp.escaped {
                Cow::Owned(String::from_utf8_lossy(bytes).into_owned())
            } else if sp.len <= 64 * 1024 {
                Cow::Owned(unescape_body(bytes))
            } else {
                let mut reader = DemandReader {
                    inner: std::io::Cursor::new(bytes),
                    source: &source,
                    state: &state,
                    since_check: 0,
                };
                Cow::Owned(fold_body(
                    &mut reader,
                    0,
                    sp.len,
                    true,
                    String::new(),
                    append_materialized,
                    String::clear,
                ))
            }
        };
        #[cfg(not(feature = "mmap"))]
        let output = {
            let mut reader = DemandReader {
                inner: source.reader(sp.offset)?,
                source: &source,
                state: &state,
                since_check: 0,
            };
            Cow::Owned(fold_body(
                &mut reader,
                sp.offset,
                sp.len,
                sp.escaped,
                String::new(),
                append_materialized,
                String::clear,
            ))
        };
        state.finish(&source)?;
        Ok(output)
    }

    /// Count decoded bytes without retaining text or swallowing source failures. The existing
    /// decoder preserves malformed-escape and lossy UTF-8 fallback; native valid unescaped spans
    /// can use their checked mapped length directly.
    pub fn try_resolved_len(&self, sp: &Span) -> anyhow::Result<usize> {
        let source = self.checked_source(sp)?;
        let state = DemandState::default();
        #[cfg(feature = "mmap")]
        let (reader, offset) = {
            let bytes = self.checked_bytes(sp, &source)?;
            if !sp.escaped && std::str::from_utf8(bytes.as_ref()).is_ok() {
                state.finish(&source)?;
                return Ok(bytes.as_ref().len());
            }
            (std::io::Cursor::new(bytes), 0)
        };
        #[cfg(not(feature = "mmap"))]
        let (reader, offset) = (source.reader(sp.offset)?, sp.offset);
        let mut reader = DemandReader {
            inner: reader,
            source: &source,
            state: &state,
            since_check: 0,
        };
        let len = fold_body(
            &mut reader,
            offset,
            sp.len,
            sp.escaped,
            0usize,
            |len, text| *len = len.saturating_add(text.len()),
            |len| *len = 0,
        );
        state.finish(&source)?;
        Ok(len)
    }

    /// Checked raw-budget prefix, preserving resolve_prefix's escape and UTF-8 cut policy.
    /// The complete original span must fit its source even when only a prefix is demanded.
    pub fn try_resolve_prefix<'a>(&'a self, sp: &Span, max_raw: u64) -> anyhow::Result<Cow<'a, str>> {
        if sp.len <= max_raw {
            return self.try_resolve(sp);
        }
        let source = self.checked_source(sp)?;
        let len = max_raw.saturating_add(3).min(sp.len);
        #[cfg(feature = "mmap")]
        let held = self.checked_bytes(&Span { len, ..sp.clone() }, &source)?;
        #[cfg(not(feature = "mmap"))]
        let held = {
            use std::io::Read;
            let mut bytes = vec![0; usize::try_from(len)?];
            let result = source.reader(sp.offset)?.read_exact(&mut bytes);
            source.check()?;
            result?;
            bytes
        };
        let bytes: &[u8] = held.as_ref();
        let end = complete_utf8_prefix(bytes, usize::try_from(max_raw)?);
        let output = Cow::Owned(if sp.escaped {
            unescape_body(&bytes[..complete_escape_prefix(&bytes[..end])])
        } else {
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        });
        source.check()?;
        Ok(output)
    }

    /// Checked bounded callbacks. Each callback borrows stable owned scratch, so it may fail
    /// or change the source without invalidating its current text argument. An error can follow
    /// earlier callbacks: discard staged output and never publish successful freshness on Err.
    pub fn try_for_each_chunk(
        &self,
        sp: &Span,
        chunk: usize,
        mut f: impl FnMut(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let source = self.checked_source(sp)?;
        let state = DemandState::default();
        let mut callback = |text: &str| {
            if state.failed() {
                return false;
            }
            let result = f(text);
            if let Err(error) = source.check() {
                state.fail(error);
            } else if let Err(error) = result {
                state.fail(error);
            }
            !state.failed()
        };
        #[cfg(feature = "mmap")]
        {
            let mut reader = DemandReader {
                inner: std::io::Cursor::new(self.checked_bytes(sp, &source)?),
                source: &source,
                state: &state,
                since_check: 0,
            };
            if stream_body(&mut reader, 0, sp.len, sp.escaped, chunk.max(1), &mut callback).is_err() && !state.failed()
            {
                state.fail(anyhow::anyhow!("Text source changed while decoding"));
            }
        }
        #[cfg(not(feature = "mmap"))]
        {
            let mut reader = DemandReader {
                inner: source.reader(sp.offset)?,
                source: &source,
                state: &state,
                since_check: 0,
            };
            if stream_body(&mut reader, sp.offset, sp.len, sp.escaped, chunk.max(1), &mut callback).is_err()
                && !state.failed()
            {
                state.fail(anyhow::anyhow!("Text source changed while decoding"));
            }
        }
        state.finish(&source)
    }
}

/// Back up a raw cap only when lookahead confirms a complete valid scalar across it. Inspecting
/// just an incomplete capped prefix would also erase malformed or truncated source bytes.
fn complete_utf8_prefix(raw: &[u8], limit: usize) -> usize {
    let end = limit.min(raw.len());
    let Some(mut start) = end.checked_sub(1) else { return 0 };
    while start > 0 && raw[start] & 0xc0 == 0x80 && end - start < 4 {
        start -= 1;
    }
    let width = match raw[start] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return end,
    };
    let scalar_end = start + width;
    if scalar_end > end && scalar_end <= raw.len() && std::str::from_utf8(&raw[start..scalar_end]).is_ok() {
        start
    } else {
        end
    }
}

/// Build a [`Span`] for a raw JSON string value `raw` (e.g. from `serde_json::value::RawValue`) that
/// borrows somewhere inside the record bytes `within` starting at absolute file offset `base_off`.
/// The span covers the string *body* (between the quotes); `escaped` is set if it contains a
/// backslash. `None` if `raw` isn't a JSON string or doesn't lie within `within`. Used by adapters'
/// span-producing parse paths (claude, codex).
#[cfg(test)]
pub fn json_string_span(raw: &RawValue, within: &[u8], base_off: u64) -> Option<Span> {
    let b = raw.get().as_bytes();
    if b.len() < 2 || b.first() != Some(&b'"') || b.last() != Some(&b'"') {
        return None;
    }
    let off = (b.as_ptr() as usize).checked_sub(within.as_ptr() as usize)?;
    if off.checked_add(b.len())? > within.len() {
        return None;
    }
    let body = &b[1..b.len() - 1];
    Some(Span {
        source: None,
        offset: base_off.checked_add(off as u64)?.checked_add(1)?,
        len: body.len() as u64,
        escaped: body.contains(&b'\\'),
    })
}

/// A serde_json `RawValue`, re-exported so adapters' span paths can name the type without depending
/// on the `raw_value` feature surface directly.
pub use serde_json::value::RawValue;

/// Keep compressed results compact, then grow geometrically from fixed capacities rather than
/// from the escape-dependent size of the first decoded source window.
fn append_materialized(output: &mut String, text: &str) {
    let needed = output
        .len()
        .checked_add(text.len())
        .expect("materialized text exceeds address space");
    if needed > output.capacity() {
        let capacity = if needed <= 64 * 1024 {
            needed
        } else {
            needed.checked_next_power_of_two().unwrap_or(needed)
        };
        output.reserve_exact(capacity - output.len());
    }
    output.push_str(text);
}

/// Fold retractable output in one decode pass. A malformed JSON body discards decoded output
/// and rereads only the span as raw lossy UTF-8; no whole source buffer is needed in either pass.
fn fold_body<R: std::io::BufRead + std::io::Seek, T>(
    reader: &mut R,
    start: u64,
    len: u64,
    escaped: bool,
    mut output: T,
    mut add: impl FnMut(&mut T, &str),
    mut reset: impl FnMut(&mut T),
) -> T {
    use std::io::{Read, SeekFrom};
    if escaped {
        let mut body = reader.by_ref().take(len);
        let decoded = decode_body(&mut body, &mut |text| {
            add(&mut output, text);
            true
        });
        if decoded.is_ok() {
            return output;
        }
        reset(&mut output);
        if reader.seek(SeekFrom::Start(start)).is_err() {
            return output;
        }
    }
    let mut body = reader.by_ref().take(len);
    stream_lossy_utf8(&mut body, &mut |text| add(&mut output, text));
    output
}

/// Decode using a bounded output buffer and, when necessary, a validation pass. Validation
/// cannot emit: a malformed escape at EOF makes the resolver return the *entire* raw body,
/// including otherwise valid escapes at its head.
fn stream_body<R: std::io::BufRead + std::io::Seek>(
    reader: &mut R,
    start: u64,
    len: u64,
    escaped: bool,
    chunk: usize,
    f: &mut impl FnMut(&str) -> bool,
) -> Result<(), ()> {
    use std::io::{Read, SeekFrom};
    let decode = escaped && validate_body(&mut reader.by_ref().take(len), true).unwrap_or(false);
    reader.seek(SeekFrom::Start(start)).map_err(|_| ())?;
    let mut output = ChunkOutput {
        text: String::with_capacity(
            chunk
                .saturating_add(3)
                .min(usize::try_from(len).unwrap_or(usize::MAX).saturating_mul(3)),
        ),
        chunk,
        f,
    };
    let mut body = reader.by_ref().take(len);
    if decode {
        decode_body(&mut body, &mut |text| output.push(text))?;
    } else {
        stream_lossy_utf8_until(&mut body, &mut |text| output.push(text));
    }
    output.flush();
    Ok(())
}

struct ChunkOutput<'a, F> {
    text: String,
    chunk: usize,
    f: &'a mut F,
}
impl<F: FnMut(&str) -> bool> ChunkOutput<'_, F> {
    fn push(&mut self, mut text: &str) -> bool {
        while !text.is_empty() {
            let mut end = (self.chunk - self.text.len()).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            self.text.push_str(&text[..end]);
            if self.text.len() >= self.chunk && !self.flush() {
                return false;
            }
            text = &text[end..];
        }
        true
    }
    fn flush(&mut self) -> bool {
        if self.text.is_empty() {
            return true;
        }
        let keep_going = (self.f)(&self.text);
        self.text.clear();
        keep_going
    }
}

fn peek_byte(reader: &mut impl std::io::BufRead) -> Option<u8> {
    reader.fill_buf().ok()?.first().copied()
}

/// Read exactly one scalar, consuming the same invalid subsequence as `from_utf8_lossy`.
/// A byte that starts the next scalar is left in the reader, including after an invalid prefix.
fn read_scalar(reader: &mut impl std::io::BufRead) -> Result<char, ()> {
    let mut bytes = [0; 4];
    let mut n = 0;
    while let Some(byte) = peek_byte(reader) {
        bytes[n] = byte;
        n += 1;
        match std::str::from_utf8(&bytes[..n]) {
            Ok(s) => {
                reader.consume(1);
                return Ok(s.chars().next().unwrap());
            }
            Err(e) => match e.error_len() {
                Some(invalid) => {
                    if invalid == n {
                        reader.consume(1);
                    }
                    return Err(());
                }
                None => reader.consume(1),
            },
        }
    }
    Err(())
}

fn stream_lossy_utf8(reader: &mut impl std::io::BufRead, f: &mut impl FnMut(&str)) {
    stream_lossy_utf8_until(reader, &mut |text| {
        f(text);
        true
    });
}

fn stream_lossy_utf8_until(reader: &mut impl std::io::BufRead, f: &mut impl FnMut(&str) -> bool) {
    loop {
        let Ok(bytes) = reader.fill_buf() else { return };
        if bytes.is_empty() {
            return;
        }
        let valid = match std::str::from_utf8(bytes) {
            Ok(s) => {
                if !f(s) {
                    return;
                }
                s.len()
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: `valid_up_to` is the validated UTF-8 prefix.
                if !f(unsafe { std::str::from_utf8_unchecked(&bytes[..valid]) }) {
                    return;
                }
                valid
            }
        };
        reader.consume(valid);
        if valid == 0 {
            let scalar = read_scalar(reader).unwrap_or('\u{fffd}');
            if !f(scalar.encode_utf8(&mut [0; 4])) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn public_span_operations_reject_overflowing_offsets() {
        let raw: &super::RawValue = serde_json::from_str("\"text\"").unwrap();
        assert!(super::json_string_span(raw, raw.get().as_bytes(), u64::MAX).is_none());
        let text = super::Text::Span(super::Span {
            source: None,
            offset: u64::MAX,
            len: 1,
            escaped: false,
        });
        assert!(text.with_skipped_prefix(1).is_err());
    }

    #[test]
    fn checked_mapped_demands_expose_only_one_validation_window() {
        use std::io::BufRead;
        let world = crate::test_world("checked-demand-window");
        let path = world.base.join("source");
        let mut body = "x".repeat(super::DEMAND_WINDOW - 1);
        body.push_str("🌙tail");
        std::fs::write(&path, &body).unwrap();
        let source = super::CheckedSource::open(&path).unwrap();
        let state = super::DemandState::default();
        let mut reader = super::DemandReader {
            inner: std::io::Cursor::new(body.as_bytes()),
            source: &source,
            state: &state,
            since_check: 0,
        };
        assert_eq!(reader.fill_buf().unwrap().len(), super::DEMAND_WINDOW);
        let mut decoded = String::new();
        super::stream_lossy_utf8(&mut reader, &mut |part| decoded.push_str(part));
        assert_eq!(decoded, body);
        state.finish(&source).unwrap();
    }

    use super::*;

    struct CheckedTestDir(PathBuf);
    impl CheckedTestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cv-checked-text-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for CheckedTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn checked_lengths_match_owned_decode_and_malformed_fallback() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("length");
        for (raw, escaped) in [
            ("plain🌙".repeat(16_384).into_bytes(), false),
            (br#"plain\n\t\"\\\uD83C\uDF19"#.repeat(16_384), true),
            ([br#"valid\n"#.repeat(16_384), br#"\q"#.to_vec()].concat(), true),
            ([vec![b'x'; 131_072], vec![0xff, 0xfe]].concat(), false),
        ] {
            std::fs::write(&path, &raw).unwrap();
            let resolver = Resolver::new(Some(path.clone()));
            let span = Span {
                offset: 0,
                len: raw.len() as u64,
                escaped,
                source: None,
            };
            let expected = if escaped {
                unescape_body(&raw)
            } else {
                String::from_utf8_lossy(&raw).into_owned()
            }
            .len();
            assert_eq!(resolver.try_resolved_len(&span).unwrap(), expected);
            assert_eq!(Text::Span(span).try_decoded_len(&resolver).unwrap(), expected);
        }
    }

    #[test]
    fn checked_demands_reject_missing_truncated_and_overflowing_sources() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        let resolver = Resolver::new(Some(path.clone()));
        let mut span = Span {
            offset: 0,
            len: 5,
            escaped: false,
            source: None,
        };
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(resolver.try_resolve_prefix(&span, 1).is_err());
        assert!(resolver.try_for_each_chunk(&span, 2, |_| Ok(())).is_err());
        std::fs::write(&path, b"abc").unwrap();
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(resolver.try_resolve_prefix(&span, 0).is_err());
        assert!(resolver.try_for_each_chunk(&span, 2, |_| Ok(())).is_err());
        span.offset = u64::MAX;
        span.len = 1;
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(Resolver::new(None).try_resolve(&span).is_err());
        std::fs::write(&path, b"").unwrap();
        span.offset = 0;
        span.len = 0;
        assert_eq!(resolver.try_resolve(&span).unwrap(), "");
        assert_eq!(resolver.try_resolve_prefix(&span, 0).unwrap(), "");
        let mut calls = 0;
        resolver
            .try_for_each_chunk(&span, 1, |_| {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 0);
    }

    #[test]
    fn checked_callbacks_keep_owned_text_and_reject_mutation_before_more_reads() {
        for escaped in [false, true] {
            let dir = CheckedTestDir::new();
            let path = dir.path().join("text");
            let raw = if escaped {
                "body\\n".repeat(40_000)
            } else {
                "body\n".repeat(40_000)
            };
            std::fs::write(&path, &raw).unwrap();
            let resolver = Resolver::new(Some(path.clone()));
            let span = Span {
                offset: 0,
                len: raw.len() as u64,
                escaped,
                source: None,
            };
            let mut calls = 0;
            let error = resolver
                .try_for_each_chunk(&span, 4096, |text| {
                    calls += 1;
                    std::fs::write(&path, b"changed").unwrap();
                    // The current callback remains readable after its mapped file is truncated.
                    assert_eq!(text.as_bytes().iter().filter(|&&b| b == b'\n').count(), 819);
                    anyhow::bail!("callback sentinel")
                })
                .unwrap_err();
            assert_eq!(calls, 1);
            assert!(!error.to_string().contains("callback sentinel"), "{error:#}");
        }
    }

    #[test]
    fn checked_chunks_preserve_decode_fallback_and_callback_errors() {
        let cases: &[(&[u8], bool)] = &[
            (b"plain", false),
            (b"a\\n\\ud83c\\udf19\\\"b", true),
            (b"a\\n\\ud83c", true),
            (b"a\\nb\\q", true),
            (b"a\xffb\xe2\x80", false),
            (b"a\\n\xffb", true),
        ];
        for &(raw, escaped) in cases {
            let dir = CheckedTestDir::new();
            let path = dir.path().join("text");
            std::fs::write(&path, raw).unwrap();
            let resolver = Resolver::new(Some(path));
            let span = Span {
                offset: 0,
                len: raw.len() as u64,
                escaped,
                source: None,
            };
            let expected = if escaped {
                unescape_body(raw)
            } else {
                String::from_utf8_lossy(raw).into_owned()
            };
            assert_eq!(resolver.try_resolve(&span).unwrap(), expected);
            for chunk in [0, 1, 2, 7, 65536] {
                let mut text = String::new();
                resolver
                    .try_for_each_chunk(&span, chunk, |part| {
                        assert!(part.len() <= chunk.max(1) + 3);
                        text.push_str(part);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(text, expected);
            }
            let mut calls = 0;
            let error = resolver
                .try_for_each_chunk(&span, 1, |_| {
                    calls += 1;
                    anyhow::bail!("callback sentinel")
                })
                .unwrap_err();
            assert_eq!(calls, 1);
            assert_eq!(error.to_string(), "callback sentinel");
        }
    }

    #[test]
    fn checked_optional_head_distinguishes_absence_from_read_failure() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        assert_eq!(read_head_checked(&path, 8).unwrap(), PersistedHead::Absent);
        assert!(matches!(
            read_head_checked(dir.path(), 8).unwrap(),
            PersistedHead::Unreadable(_)
        ));
        std::fs::write(&path, b"a\xffb").unwrap();
        for cap in 0..5 {
            assert_eq!(
                read_head_checked(&path, cap).unwrap(),
                PersistedHead::Text(read_head(&path, cap).unwrap())
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway; everyone else gets a noted, unreadable output.
            if std::fs::File::open(&path).is_err() {
                let head = read_head_checked(&path, 8).unwrap();
                assert!(
                    matches!(&head, PersistedHead::Unreadable(reason) if reason.contains("ermission")),
                    "{head:?}"
                );
                assert!(head
                    .indexed_text("/p")
                    .unwrap()
                    .starts_with("[persisted output /p unreadable: "));
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let captured = crate::json::JsonSource::capture(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(CheckedSource::open_captured(path, captured).is_err());
    }

    #[test]
    fn checked_demands_match_independent_large_string_oracle_and_source_overrides() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        let content = "é🌙 quote\" slash\\ newline\n\u{2003}".repeat(4000);
        let quoted = serde_json::to_string(&content).unwrap();
        let raw = &quoted.as_bytes()[1..quoted.len() - 1];
        let mut file = b"prefix!".to_vec();
        file.extend_from_slice(raw);
        file.extend_from_slice(b"suffix!");
        std::fs::write(&path, &file).unwrap();
        let span = Span {
            source: Some(path.clone()),
            offset: 7,
            len: raw.len() as u64,
            escaped: true,
        };
        let resolver = Resolver::new(Some(dir.path().join("wrong-primary")));
        assert_eq!(resolver.try_resolve(&span).unwrap(), content);
        for chunk in [1, 2, 13, 65536] {
            let mut output = String::new();
            resolver
                .try_for_each_chunk(&span, chunk, |part| {
                    assert!(part.len() <= chunk + 3);
                    output.push_str(part);
                    Ok(())
                })
                .unwrap();
            assert_eq!(output, content);
        }
        // A malformed tail after valid decoder windows must retract the entire decoded prefix.
        let late_path = dir.path().join("late");
        let mut malformed = raw.to_vec();
        malformed.extend_from_slice(b"\\ud83c");
        std::fs::write(&late_path, &malformed).unwrap();
        let span = Span {
            source: Some(late_path),
            offset: 0,
            len: malformed.len() as u64,
            escaped: true,
        };
        assert_eq!(
            resolver.try_resolve(&span).unwrap(),
            String::from_utf8_lossy(&malformed)
        );
        let mut output = String::new();
        resolver
            .try_for_each_chunk(&span, 8192, |part| {
                output.push_str(part);
                Ok(())
            })
            .unwrap();
        assert_eq!(output, String::from_utf8_lossy(&malformed));
        // Literal Unicode whitespace cut mid-scalar must remain whitespace, including portable.
        let whitespace_path = dir.path().join("whitespace");
        let whitespace = format!("{}wanted", "\u{2003}".repeat(10_000));
        std::fs::write(&whitespace_path, &whitespace).unwrap();
        let span = Span {
            source: Some(whitespace_path),
            offset: 0,
            len: whitespace.len() as u64,
            escaped: false,
        };
        let prefix = resolver.try_resolve_prefix(&span, 1024).unwrap();
        assert_eq!(prefix.len(), 1023);
        assert!(prefix.chars().all(char::is_whitespace));
    }

    /// A transcript renamed over after the parse, before any demand, must not resolve: the
    /// session's spans point into the file that was parsed, not into its replacement.
    /// A whole read of a transcript many windows long, every span resolved, never maps more than
    /// a window (grown to hold its longest line) or a span at a time: mapped bytes are bounded by
    /// the read's shape, not the file's size.
    #[cfg(feature = "mmap")]
    #[test]
    fn reading_a_large_transcript_maps_a_bounded_window_never_the_file() {
        use crate::harness::store::jsonl::{MAPPED_PEAK, MAP_WINDOW};
        use crate::ir::{Harness, SessionRef};
        let world = cv_testkit::World::new("lazy-bounded-mapping");
        let path = world.proj.join("windowed.jsonl");
        let line = |i: usize, len: usize| {
            serde_json::json!({"type": "user", "uuid": format!("u{i}"), "sessionId": "windowed",
                "message": {"role": "user", "content": format!("{i} {}", "x".repeat(len))}})
            .to_string()
                + "\n"
        };
        let longest = 3 * MAP_WINDOW as usize;
        let body: String = (0..400)
            .map(|i| line(i, if i == 200 { longest } else { 9_000 }))
            .collect();
        std::fs::write(&path, &body).unwrap();
        MAPPED_PEAK.with(|peak| peak.set(0));
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.proj);
        let reference = SessionRef::new("windowed", Harness::Claude, path.clone());
        let mut session =
            crate::stream::collect_with(&adapter, &reference, &crate::stream::ParseOptions::lazy()).unwrap();
        assert!(session.messages.iter().any(|m| m.content.iter().any(|b| matches!(b,
            crate::ir::Block::Text { text } if text.is_span()))));
        session.materialize().unwrap();
        assert_eq!(session.messages.len(), 400);
        let peak = MAPPED_PEAK.with(|peak| peak.get());
        assert!(peak > 0, "the read was mapped");
        assert!(
            peak <= 2 * (longest + 1024),
            "peak mapping {peak} for a {longest}-byte line"
        );
        assert!(
            peak * 4 < body.len(),
            "peak mapping {peak} of a {}-byte file",
            body.len()
        );
    }

    #[test]
    fn a_file_replaced_after_the_parse_fails_the_first_demand_and_growth_does_not() {
        use crate::ir::{Block, Harness, SessionRef};
        use std::io::Write;
        let world = cv_testkit::World::new("lazy-parse-stamp");
        let path = world.proj.join("stamped.jsonl");
        let line = |fill: &str| {
            serde_json::json!({"type": "user", "uuid": fill, "sessionId": "stamped",
                "message": {"role": "user", "content": fill.repeat(10_000)}})
            .to_string()
                + "\n"
        };
        let parse = || {
            let adapter = crate::harness::claude::Claude::with_config_dir(&world.proj);
            let reference = SessionRef::new("stamped", Harness::Claude, path.clone());
            crate::stream::collect_with(&adapter, &reference, &crate::stream::ParseOptions::lazy()).unwrap()
        };
        let first_text = |session: &crate::Session| match &session.messages[0].content[0] {
            Block::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        };
        std::fs::write(&path, line("A")).unwrap();
        let session = parse();
        assert!(first_text(&session).is_span());
        let replacement = world.base.join("replacement");
        std::fs::write(&replacement, line("B")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let error = session.clone().materialize().unwrap_err();
        assert!(error.to_string().contains("replaced or rewritten"), "{error:#}");
        let resolver = session.resolver();
        assert!(first_text(&session).try_resolve(&resolver).is_err());
        // An unguarded streaming sink's resolver is built before the parse: same answer.
        let early = Resolver::new(Some(path.clone()));
        let session = parse();
        std::fs::write(&replacement, line("C")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(first_text(&session).try_resolve(&early).is_err());
        // A live transcript only grows: appending after the parse still resolves.
        std::fs::write(&path, line("A")).unwrap();
        let mut session = parse();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(line("D").as_bytes())
            .unwrap();
        session.materialize().unwrap();
        assert_eq!(&*first_text(&session), "A".repeat(10_000));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn checked_mapping_rejects_replaced_path_after_first_demand() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        std::fs::write(&path, b"before").unwrap();
        let resolver = Resolver::new(Some(path.clone()));
        let span = Span {
            source: None,
            offset: 0,
            len: 6,
            escaped: false,
        };
        assert_eq!(resolver.try_resolve(&span).unwrap(), "before");
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"after!").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(resolver.try_resolve_prefix(&span, 2).is_err());
        assert!(resolver.try_for_each_chunk(&span, 2, |_| Ok(())).is_err());
        assert_eq!(Resolver::new(Some(path)).try_resolve(&span).unwrap(), "after!");
    }

    #[test]
    fn checked_reader_error_cannot_become_malformed_raw_fallback() {
        use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
        struct FailAfterWindow(Cursor<Vec<u8>>);
        impl BufRead for FailAfterWindow {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                if self.0.position() >= 65536 {
                    Err(std::io::Error::other("injected Text read failure"))
                } else {
                    let available = 65536 - self.0.position() as usize;
                    let bytes = self.0.fill_buf()?;
                    Ok(&bytes[..bytes.len().min(available)])
                }
            }
            fn consume(&mut self, n: usize) {
                self.0.consume(n);
            }
        }
        impl Read for FailAfterWindow {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(bytes)
            }
        }
        impl Seek for FailAfterWindow {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.0.seek(position)
            }
        }
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        let body = "body\\n".repeat(40_000).into_bytes();
        std::fs::write(&path, &body).unwrap();
        let source = CheckedSource::open(&path).unwrap();
        let state = DemandState::default();
        let mut reader = DemandReader {
            inner: FailAfterWindow(Cursor::new(body.clone())),
            source: &source,
            state: &state,
            since_check: 0,
        };
        let mut calls = 0;
        assert!(stream_body(&mut reader, 0, body.len() as u64, true, 8192, &mut |_| {
            calls += 1;
            true
        })
        .is_err());
        assert_eq!(calls, 0);
        let error = state.finish(&source).unwrap_err();
        assert_eq!(error.to_string(), "injected Text read failure");
    }

    fn chunked_body(raw: &[u8], escaped: bool, input_window: usize, chunk: usize) -> String {
        let mut reader = std::io::BufReader::with_capacity(input_window, std::io::Cursor::new(raw));
        let mut result = String::new();
        stream_body(&mut reader, 0, raw.len() as u64, escaped, chunk.max(1), &mut |s| {
            assert!(!s.is_empty());
            assert!(s.len() <= chunk.max(1).saturating_add(3), "chunk {chunk}: {s:?}");
            result.push_str(s);
            true
        })
        .unwrap();
        result
    }

    #[test]
    fn chunks_match_serde_across_escape_and_unicode_boundaries() {
        let mut bodies = vec![
            r#"\"\\\/\b\f\n\r\t\u0000\u007f\u0080\u07ff\u0800\uffff\uD834\uDD1E"#.to_owned(),
            r#"\\\\\\\"\\n\n\\u0041\u0041"#.to_owned(),
            "ASCII é λ 世 😀 mixed".to_owned(),
            String::new(),
        ];
        let source = "quote \" slash \\ controls \u{0000}\u{0008}\u{000c}\n\r\t / é λ 世 😀";
        let quoted = serde_json::to_string(source).unwrap();
        bodies.push(quoted[1..quoted.len() - 1].to_owned());
        for body in bodies {
            for padding in 0..13 {
                let body = format!("{}{body}tail", "x".repeat(padding));
                let expected: String = serde_json::from_str(&format!("\"{body}\"")).unwrap();
                for window in 1..18 {
                    for chunk in 0..18 {
                        assert_eq!(
                            chunked_body(body.as_bytes(), true, window, chunk),
                            expected,
                            "window={window}, chunk={chunk}, body={body:?}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn oversized_chunk_budget_still_emits_a_small_field() {
        assert_eq!(chunked_body(b"a\\nb", true, 1, usize::MAX), "a\nb");
        assert_eq!(chunked_body(b"\xff", false, 1, usize::MAX), "\u{fffd}");
    }

    #[test]
    fn chunks_preserve_json_windows_at_every_escape_and_unicode_cut() {
        let suffixes = [
            r#"é😀\"\\\/\b\f\n\r\t\u0041\uD834\uDD1Etail"#.to_owned(),
            r"\uD834\uDD1Etail".to_owned(),
            format!("{}uD834\\u0041tail", "\\".repeat(200)),
            format!("{}uD834\\uDD1Etail", "\\".repeat(201)),
        ];
        for suffix in suffixes {
            for padding in 65500..65550 {
                let body = format!("{}{suffix}", "a".repeat(padding));
                let expected: String = serde_json::from_str(&format!("\"{body}\"")).unwrap();
                for window in [7, 65536] {
                    assert_eq!(chunked_body(body.as_bytes(), true, window, 7), expected);
                }
                // An invalid tail must retain raw text, including escapes decoded in prior windows.
                let malformed = format!("{body}\\q");
                assert_eq!(chunked_body(malformed.as_bytes(), true, 7, 7), malformed);
            }
        }
    }

    #[test]
    fn chunks_preserve_malformed_and_lossy_fallback() {
        let bodies: &[&[u8]] = &[
            b"good\\nthen\\q",
            b"good\\nthen\\",
            b"\\u",
            b"\\u123",
            b"\\u12xz",
            b"\\uD800",
            b"\\uDC00",
            b"\\uD800\\u0041",
            b"\\uD800x\\uDC00",
            b"good\\nthen\"quote",
            b"good\\nthen\ncontrol",
            b"good\\nthen\xff",
            b"raw\"quote",
            b"raw\ncontrol",
            b"\xe1\x80A",
            b"\xf0\x9f\x92",
            b"\xed\xa0\x80\\n",
            b"\xf4\x90\x80\x80",
            b"\xc0\xaf",
        ];
        for raw in bodies {
            for escaped in [false, true] {
                let expected = if escaped {
                    unescape_body(raw)
                } else {
                    String::from_utf8_lossy(raw).into_owned()
                };
                for window in 1..14 {
                    for chunk in 0..14 {
                        assert_eq!(
                            chunked_body(raw, escaped, window, chunk),
                            expected,
                            "window={window}, chunk={chunk}, raw={raw:?}, escaped={escaped}"
                        );
                    }
                }
            }
        }
        // Deterministic arbitrary byte sequences exercise lossy subsequence grouping, independent
        // of the hand-picked malformed JSON cases above.
        let mut seed = 0x5828_7134u32;
        for len in 0..64 {
            for _ in 0..32 {
                let raw: Vec<u8> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        seed as u8
                    })
                    .collect();
                for escaped in [false, true] {
                    let expected = if escaped {
                        unescape_body(&raw)
                    } else {
                        String::from_utf8_lossy(&raw).into_owned()
                    };
                    assert_eq!(chunked_body(&raw, escaped, 3, 2), expected);
                }
            }
        }
    }

    #[test]
    fn resolver_chunks_read_only_the_span_and_cross_source_windows() {
        let dir = std::env::temp_dir().join(format!("cv-lazy-chunks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body.txt");
        // Both the fixed non-mmap source window and tiny output windows split escapes/scalars.
        let raw = format!("{}é\\uD834\\uDD1E\\n{}😀", "a".repeat(65531), "b".repeat(65529));
        let source = format!("prefix{raw}suffix");
        std::fs::write(&path, source).unwrap();
        let resolver = Resolver::new(Some(path));
        let span = Span {
            source: None,
            offset: 6,
            len: raw.len() as u64,
            escaped: true,
        };
        let expected: String = serde_json::from_str(&format!("\"{raw}\"")).unwrap();
        for chunk in [0, 1, 2, 3, 7, 65536] {
            let mut got = String::new();
            resolver
                .try_for_each_chunk(&span, chunk, |s| {
                    assert!(s.len() <= chunk.max(1usize).saturating_add(3));
                    got.push_str(s);
                    Ok(())
                })
                .unwrap();
            assert_eq!(got, expected);
        }
        let invalid = Span {
            len: source_len(&raw) + 1,
            ..span.clone()
        };
        let mut calls = 0;
        assert!(resolver
            .try_for_each_chunk(&invalid, 1, |_| {
                calls += 1;
                Ok(())
            })
            .is_err());
        assert_eq!(calls, 0);
        drop(resolver);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn source_len(raw: &str) -> u64 {
        // The invalid span starts after "prefix", so include all remaining bytes plus one.
        raw.len() as u64 + 6
    }

    #[test]
    fn whole_resolve_matches_old_unescape_and_respects_source_bounds() {
        let dir = std::env::temp_dir().join(format!("cv-lazy-whole-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bodies = vec![
            Vec::new(),
            b"plain without escapes".to_vec(),
            b"raw quote \" and newline\n".to_vec(),
            b"invalid UTF8 \xff".to_vec(),
            br#"\"\\\/\b\f\n\r\t\u0000\u007f\u0080\u07ff\u0800\uffff\uD834\uDD1E"#.to_vec(),
            br#"valid\nthen\uD834"#.to_vec(),
            br#"valid\nthen\uDD1E"#.to_vec(),
            br#"valid\nthen\q"#.to_vec(),
            b"valid\\nthen invalid \xff".to_vec(),
        ];
        // Both sides of the one-pass/collection cutoff must decode and fall back identically.
        for size in [65535, 65536, 65537] {
            bodies.push(format!("{}\\n", "a".repeat(size - 2)).into_bytes());
            bodies.push(format!("{}\\q", "a".repeat(size - 2)).into_bytes());
        }
        // Exercise both 64KiB decoder boundaries and a malformed tail after valid windows.
        for padding in 65522..65538 {
            let raw = format!("{}é\\uD834\\uDD1E\\n{}😀", "a".repeat(padding), "b".repeat(65529));
            bodies.push(raw.as_bytes().to_vec());
            bodies.push(format!("{raw}\\u00").into_bytes());
        }
        for (i, raw) in bodies.iter().enumerate() {
            let path = dir.join(format!("body-{i}"));
            std::fs::write(&path, [b"prefix".as_slice(), raw, b"suffix"].concat()).unwrap();
            let resolver = Resolver::new(Some(dir.join("wrong-primary")));
            let span = Span {
                source: Some(path.clone()),
                offset: 6,
                len: raw.len() as u64,
                escaped: true,
            };
            // Independent old whole-body parser remains the prefix resolver's oracle.
            assert_eq!(resolver.try_resolve(&span).unwrap(), unescape_body(raw), "body {i}");
            assert_eq!(
                resolver.try_resolved_len(&span).unwrap(),
                unescape_body(raw).len(),
                "length {i}"
            );
            let literal = Span {
                escaped: false,
                ..span.clone()
            };
            assert_eq!(
                resolver.try_resolved_len(&literal).unwrap(),
                String::from_utf8_lossy(raw).len(),
                "literal length {i}"
            );
            for invalid in [
                Span {
                    offset: 6,
                    len: raw.len() as u64 + 7,
                    ..span.clone()
                },
                Span {
                    offset: u64::MAX,
                    len: 1,
                    ..span.clone()
                },
                Span {
                    len: u64::MAX,
                    ..span.clone()
                },
                Span {
                    source: Some(dir.join("missing")),
                    ..span.clone()
                },
            ] {
                assert!(resolver.try_resolve(&invalid).is_err(), "body {i}: {invalid:?}");
                assert!(resolver.try_resolved_len(&invalid).is_err(), "length {i}: {invalid:?}");
            }
        }
        let path = dir.join("ordinary");
        std::fs::write(&path, "ordinary é text").unwrap();
        let resolver = Resolver::new(Some(path));
        let ordinary = Span {
            source: None,
            offset: 0,
            len: 16,
            escaped: false,
        };
        assert_eq!(resolver.try_resolve(&ordinary).unwrap(), "ordinary é text");
        assert_eq!(resolver.try_resolved_len(&ordinary).unwrap(), 16);
        drop(resolver);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn large_compressed_materialization_keeps_its_owned_result_compact() {
        let dir = std::env::temp_dir().join(format!("cv-lazy-compressed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body");
        let raw = r"\u0061".repeat(10923);
        assert!(raw.len() > 64 * 1024);
        std::fs::write(&path, &raw).unwrap();
        let resolver = Resolver::new(Some(path));
        let span = Span {
            source: None,
            offset: 0,
            len: raw.len() as u64,
            escaped: true,
        };
        let text = resolver.try_resolve(&span).unwrap().into_owned();
        assert_eq!(text, "a".repeat(10923));
        assert!(
            text.capacity() < 2 * text.len(),
            "small decoded output must not retain a source window"
        );
        drop(resolver);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retractable_folds_decode_once_and_reset_malformed_output() {
        struct CountedReader {
            inner: std::io::Cursor<Vec<u8>>,
            consumed: usize,
            seeks: usize,
        }
        impl std::io::Read for CountedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.consumed += n;
                Ok(n)
            }
        }
        impl std::io::BufRead for CountedReader {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                self.inner.fill_buf()
            }
            fn consume(&mut self, n: usize) {
                self.inner.consume(n);
                self.consumed += n;
            }
        }
        impl std::io::Seek for CountedReader {
            fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
                self.seeks += 1;
                self.inner.seek(pos)
            }
        }
        let raw = format!(
            "{}{}",
            "a".repeat(65525),
            r#"é😀\"\\\/\b\f\n\r\t\u0000\uD834\uDD1E"#.repeat(5000)
        );
        let expected: String = serde_json::from_str(&format!("\"{raw}\"")).unwrap();
        for suffix in [b"".as_slice(), br"\q", br"\uD800", b"\xff\xf0\x90"] {
            let body = [raw.as_bytes(), suffix].concat();
            let expected = if suffix.is_empty() {
                expected.clone()
            } else {
                String::from_utf8_lossy(&body).into_owned()
            };
            let new_reader = || {
                let mut inner = std::io::Cursor::new([b"prefix".as_slice(), &body, b"suffix"].concat());
                inner.set_position(6);
                CountedReader {
                    inner,
                    consumed: 0,
                    seeks: 0,
                }
            };
            let mut reader = new_reader();
            let text = fold_body(
                &mut reader,
                6,
                body.len() as u64,
                true,
                String::new(),
                String::push_str,
                String::clear,
            );
            assert_eq!(text, expected);
            assert_eq!(reader.seeks, usize::from(!suffix.is_empty()));
            assert_eq!(reader.consumed, body.len() * (1 + usize::from(!suffix.is_empty())));
            let mut reader = new_reader();
            let len = fold_body(
                &mut reader,
                6,
                body.len() as u64,
                true,
                0usize,
                |len, s| *len += s.len(),
                |len| *len = 0,
            );
            assert_eq!(len, expected.len());
            assert_eq!(reader.seeks, usize::from(!suffix.is_empty()));
            assert_eq!(reader.consumed, body.len() * (1 + usize::from(!suffix.is_empty())));
        }
    }

    #[test]
    fn chunk_output_capacity_is_independent_of_field_size() {
        let mut bytes = 0;
        let mut accept = |s: &str| {
            bytes += s.len();
            true
        };
        let mut output = ChunkOutput {
            text: String::with_capacity(10),
            chunk: 7,
            f: &mut accept,
        };
        let capacity = output.text.capacity();
        // A giant logical field built from a small source window never grows the output buffer.
        for _ in 0..1_000_000 {
            output.push("é😀a");
            assert_eq!(output.text.capacity(), capacity);
        }
        output.flush();
        assert_eq!(bytes, 7_000_000);
    }

    #[test]
    fn inline_roundtrips_as_string() {
        let t = Text::from("hello");
        assert_eq!(&*t, "hello");
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"hello\"");
        let back: Text = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(back, Text::Inline("hello".into()));
    }

    /// `resolve_prefix` reads only the head of a span — including backing an escaped cut up to a
    /// complete escape so the head unescapes exactly like the full resolve's head would.
    #[test]
    fn resolve_prefix_caps_and_respects_escapes() {
        let dir = std::env::temp_dir().join(format!("cv-lazy-prefix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body.txt");
        // Raw JSON string body: "line1\nline2…" with the escape right at the cut boundary.
        std::fs::write(&path, b"line one\\nline two and much more after").unwrap();
        let resolver = Resolver::new(Some(path.clone()));

        let span = |len: u64, escaped: bool| Span {
            source: None,
            offset: 0,
            len,
            escaped,
        };
        let full_len = std::fs::metadata(&path).unwrap().len();

        // Unescaped: plain byte cap.
        assert_eq!(
            resolver.try_resolve_prefix(&span(full_len, false), 8).unwrap(),
            "line one"
        );
        // Cap ≥ len: identical to a full resolve.
        assert_eq!(
            resolver
                .try_resolve_prefix(&span(full_len, true), full_len + 10)
                .unwrap(),
            resolver.try_resolve(&span(full_len, true)).unwrap()
        );
        // Escaped, cut mid-`\n` (raw bytes 9 ends between '\' and 'n'): backs up to byte 8.
        assert_eq!(
            resolver.try_resolve_prefix(&span(full_len, true), 9).unwrap(),
            "line one"
        );
        // Escaped, cut after the complete escape: the newline survives.
        assert_eq!(
            resolver.try_resolve_prefix(&span(full_len, true), 14).unwrap(),
            "line one\nline"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_prefix_preserves_unicode_boundaries_and_malformed_source() {
        let dir = std::env::temp_dir().join(format!("cv-lazy-prefix-unicode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let valid = format!("{}wanted é 😀", "\u{2003}".repeat(10000));
        let malformed: &[&[u8]] = &[
            b"\xff\xe2\x80Awanted",
            b"\xff\xe2\x80\x83wanted",
            b"\xe2\x80",
            b"\xf0\x90\x80",
            b"\xe0\x80\x80wanted",
            b"\xed\xa0\x80wanted",
            b"\xf4\x90\x80\x80wanted",
            b"\x80\x80\x80\x80\x80wanted",
        ];
        let bodies: Vec<&[u8]> = std::iter::once(valid.as_bytes())
            .chain(malformed.iter().copied())
            .collect();
        for (i, body) in bodies.iter().enumerate() {
            let path = dir.join(i.to_string());
            // Offset and suffix make lookahead's original-span boundary observable.
            std::fs::write(&path, [b"prefix".as_slice(), body, b"\x83outside"].concat()).unwrap();
            let resolver = Resolver::new(Some(path));
            for escaped in [false, true] {
                let span = Span {
                    source: None,
                    offset: 6,
                    len: body.len() as u64,
                    escaped,
                };
                let caps: Vec<usize> = if i == 0 {
                    (0..8)
                        .chain([
                            1024,
                            body.len() - 4,
                            body.len() - 3,
                            body.len() - 2,
                            body.len() - 1,
                            body.len(),
                        ])
                        .collect()
                } else {
                    (0..=body.len()).collect()
                };
                for cap in caps {
                    // Independent scalar iteration identifies valid scalars straddling the cap;
                    // invalid UTF-8 subsequences keep the existing lossy capped representation.
                    let mut keep = cap;
                    let mut offset = 0;
                    for chunk in body.utf8_chunks() {
                        for (start, ch) in chunk.valid().char_indices() {
                            let start = offset + start;
                            if start < cap && cap < start + ch.len_utf8() {
                                keep = start;
                            }
                        }
                        offset += chunk.valid().len() + chunk.invalid().len();
                    }
                    assert_eq!(
                        resolver.try_resolve_prefix(&span, cap as u64).unwrap(),
                        String::from_utf8_lossy(&body[..keep]),
                        "body {i}, cap {cap}, escaped {escaped}"
                    );
                }
                if i == 0 {
                    assert!(resolver.try_resolve_prefix(&span, 1024).unwrap().trim().is_empty());
                    assert_eq!(resolver.try_resolve(&span).unwrap(), valid);
                }
            }
        }
        // A span the source cannot hold fails even when the requested prefix is readable.
        let path = dir.join("short-source");
        std::fs::write(&path, b"\xe2\x80").unwrap();
        let resolver = Resolver::new(Some(path));
        let span = Span {
            source: None,
            offset: 0,
            len: 8,
            escaped: false,
        };
        assert!(resolver.try_resolve_prefix(&span, 1).is_err());
        drop(resolver);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
