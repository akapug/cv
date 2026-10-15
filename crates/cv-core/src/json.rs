//! Complete JSON values with explicit owned and read-only source representations.
//!
//! Generic Serde serialization always emits the actual value, resolving a selected source value
//! when necessary. Consumers that only need projections or compact serialized length can use
//! [`Json::with_view`], [`Json::canonical_len`] and [`Json::write_canonical`] without constructing
//! a descendant value tree. Encoded argument documents use operation-owned decoded backing.
//!
//! Native source views borrow mapped bytes. Without `mmap`, positioned reads retain only bounded
//! input windows shared by all projections in an operation. The canonical walker bounds decoded
//! string scratch in both builds;
//! object key/order state grows with object width. Neither limitation is hidden by this API.

mod transport;
pub(crate) use transport::{SourceDescriptor, SourceRegistry};

use crate::harness::store::document;
use crate::harness::store::document::range::{Range, RangeSource};
use crate::harness::store::document::Raw;
#[cfg(feature = "mmap")]
use crate::harness::store::document::Source;
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use std::borrow::Cow;
use std::fs::Metadata;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A complete JSON value. Source descriptors clone cheaply; owned values retain Value semantics.
/// Deserialization always produces `Owned`, including objects that resemble source descriptors.
#[derive(Clone, Debug)]
pub enum Json {
    Owned(Value),
    Source(Arc<JsonRange>),
}

/// A validated complete value range, tied to a read-only source identity. Fields are private so
/// callers cannot create an unchecked range. Normal edits, appends and replacement invalidate it.
/// Metadata checks do not provide an immutable snapshot against arbitrary external in-place
/// rewrites or prevent external truncation of a mapped file.
#[derive(Debug)]
pub struct JsonRange {
    source: JsonSource,
    offset: u64,
    len: u64,
    decoded: Option<Arc<crate::harness::store::bytes::ByteSource>>,
}

/// Distinguish invalid content (archives historically appear empty) from unavailable or changed
/// sources, whose failures must abort consumers and cannot publish freshness.
#[derive(Debug)]
pub(crate) struct InvalidJson;
impl std::fmt::Display for InvalidJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid JSON document")
    }
}
impl std::error::Error for InvalidJson {}

/// Capture before an adapter validates its source. Only already accepted complete values may use
/// validated_range afterward; this avoids validating each giant input again at producer handoff.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct JsonSource(Arc<CapturedSource>);
#[derive(Debug)]
struct CapturedSource {
    path: PathBuf,
    identity: Identity,
    /// Line-delimited logs are append-only by contract: a writer may still be adding records
    /// after capture. Growth of the same file leaves the captured prefix intact, so it is not a
    /// change; truncation, replacement and same-length rewrites still are.
    appendable: bool,
    reads: Option<Arc<crate::source::ReadSet>>,
    decoded: std::sync::Mutex<Option<Arc<crate::harness::store::bytes::ByteSource>>>,
}
impl PartialEq for CapturedSource {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.identity == other.identity && self.reads == other.reads
    }
}
impl Eq for CapturedSource {}
impl std::hash::Hash for CapturedSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.identity.hash(state);
        self.reads.hash(state);
    }
}
/// Extensions of line-delimited logs that harnesses append to while a session is live.
const APPEND_ONLY_EXTENSIONS: &[&str] = &["jsonl", "ndjson"];
fn is_append_only_log(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| APPEND_ONLY_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl Identity {
    fn admits(&self, later: &Identity) -> bool {
        if later.len == self.len {
            return later == self;
        }
        #[cfg(unix)]
        return later.len > self.len && later.device == self.device && later.inode == self.inode;
        #[cfg(not(unix))]
        return later.len > self.len;
    }
    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path).with_context(|| format!("reading JSON source {}", path.display()))?;
        Self::from_metadata(metadata)
    }
    fn from_metadata(metadata: Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(metadata.is_file(), "JSON source is not a regular file");
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}
impl JsonSource {
    pub(crate) fn decoded_backing(&self) -> Result<Arc<crate::harness::store::bytes::ByteSource>> {
        let mut decoded = self
            .0
            .decoded
            .lock()
            .map_err(|_| std::io::Error::other("decoded backing poisoned"))?;
        if let Some(bytes) = decoded.as_ref() {
            return Ok(bytes.clone());
        }
        let bytes = Arc::new(crate::harness::store::bytes::ByteSource::arena()?);
        *decoded = Some(bytes.clone());
        Ok(bytes)
    }

    pub(crate) fn capture(path: &Path) -> Result<Self> {
        Self::capture_with_reads(path, &crate::source::ReadSet::default())
    }
    pub(crate) fn capture_with_reads(path: &Path, reads: &crate::source::ReadSet) -> Result<Self> {
        let source = Self::capture_guarded(path, Some(Arc::new(reads.clone())))?;
        source.check()?;
        Ok(source)
    }
    fn capture_guarded(path: &Path, reads: Option<Arc<crate::source::ReadSet>>) -> Result<Self> {
        // Preserve symlink spelling so later retargeting changes the observed identity.
        let path = std::path::absolute(path).context("locating JSON source")?;
        let identity = Identity::read(&path)?;
        Ok(Self(Arc::new(CapturedSource {
            appendable: is_append_only_log(&path),
            path,
            identity,
            reads,
            decoded: std::sync::Mutex::new(None),
        })))
    }
    pub(crate) fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.holds(&Identity::read(&self.0.path)?),
            "JSON source changed: {}",
            self.0.path.display()
        );
        self.check_reads()
    }
    /// Whether the file as observed now still carries every byte captured: identical, or for an
    /// append-only log the same file grown.
    fn holds(&self, now: &Identity) -> bool {
        if self.0.appendable {
            self.0.identity.admits(now)
        } else {
            *now == self.0.identity
        }
    }
    fn check_reads(&self) -> Result<()> {
        if let Some(reads) = &self.0.reads {
            anyhow::ensure!(
                reads.fresh(&mut crate::source::Probe::default()),
                "JSON source dependencies changed or unavailable: {}",
                self.0.path.display()
            );
        }
        Ok(())
    }
    /// [`Self::capture`] without the dependency check: one stat, for a parse-time stamp.
    pub(crate) fn capture_identity(path: &Path) -> Result<Self> {
        Self::capture_guarded(path, None)
    }
    /// `later` is this source unchanged, or this source only grown: the append contract resume
    /// marks also rely on. On unix the device and inode must match; an unchanged length must
    /// match exactly (mtime and ctime included). An in-place rewrite that also grows the file
    /// keeps its inode and is not detected here.
    pub(crate) fn admits(&self, later: &JsonSource) -> bool {
        self.0.path == later.0.path && self.0.identity.admits(&later.0.identity)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
    pub(crate) fn len(&self) -> u64 {
        self.0.identity.len
    }
    pub(crate) fn check_metadata(&self, metadata: Metadata) -> Result<()> {
        anyhow::ensure!(
            self.holds(&Identity::from_metadata(metadata)?),
            "JSON source changed: {}",
            self.0.path.display()
        );
        self.check_reads()
    }
    fn range(&self, offset: u64, len: u64) -> Result<JsonRange> {
        anyhow::ensure!(len != 0, "JSON range is empty");
        anyhow::ensure!(
            offset.checked_add(len).is_some_and(|end| end <= self.0.identity.len),
            "JSON range is out of bounds"
        );
        Ok(JsonRange {
            source: self.clone(),
            offset,
            len,
            decoded: None,
        })
    }
    /// Project an already mapped, UTF8-checked original-source range without mapping it again.
    /// The producer checks its opened descriptor against this capture before supplying bytes.
    #[cfg(feature = "mmap")]
    pub(crate) fn read_borrowed_range_value<T>(
        &self,
        offset: u64,
        text: &str,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.read_borrowed_range(offset, text, Validation::Value, f)
    }
    /// Syntax-only counterpart for typed projections whose ignored fields use IgnoredAny.
    #[cfg(feature = "mmap")]
    pub(crate) fn read_borrowed_range_syntax<T>(
        &self,
        offset: u64,
        text: &str,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.read_borrowed_range(offset, text, Validation::Syntax, f)
    }
    #[cfg(feature = "mmap")]
    fn read_borrowed_range<T>(
        &self,
        offset: u64,
        text: &str,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let range = self.range(offset, text.len() as u64)?;
        self.check()?;
        let index = document::Index::default();
        let result = range.with_native_view(Raw::new(text, &index), validation, f);
        self.check()?;
        result
    }
    /// One Value-compatible record of an in-memory copy (mapped or read) of this capture. No
    /// stat: the record reader checks identity once per operation and after each record that
    /// published a source descriptor, rather than twice per record.
    pub(crate) fn read_record<T>(
        &self,
        offset: u64,
        text: &str,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let index = document::Index::default();
        let raw = Raw::new(text, &index);
        if !document::validate(raw) {
            return Err(InvalidJson.into());
        }
        self.range(offset, text.len() as u64)?
            .with_native_view(raw, Validation::Cached, f)
    }
    /// [`Self::read_record`] over this source's private decoded bytes (`decoded_len` of them), an
    /// in-memory copy of one record at `offset`. Decoded bytes are immutable: the reader checks
    /// the original when it decodes and when its pass ends, never per record.
    pub(crate) fn read_decoded_record<T>(
        &self,
        backing: &Arc<crate::harness::store::bytes::ByteSource>,
        decoded_len: u64,
        offset: u64,
        text: &str,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let index = document::Index::default();
        let raw = Raw::new(text, &index);
        if !document::validate(raw) {
            return Err(InvalidJson.into());
        }
        self.decoded_range(backing.clone(), decoded_len, offset, text.len() as u64)?
            .with_native_view(raw, Validation::Cached, f)
    }
    /// [`Self::read_positioned_range_value`] over this source's private decoded bytes, with a
    /// context made by `RangeSource::decoded` over the same backing.
    pub(crate) fn read_positioned_decoded_value<T>(
        &self,
        context: &RangeSource,
        backing: &Arc<crate::harness::store::bytes::ByteSource>,
        decoded_len: u64,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.decoded_range(backing.clone(), decoded_len, offset, len)?
            .with_positioned_view(context, Validation::Value, f)
    }
    /// Reuse one checked original-source positioned context for many typed records.
    #[cfg(not(feature = "mmap"))]
    pub(crate) fn read_positioned_range_syntax<T>(
        &self,
        context: &RangeSource,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            context.matches_original(self),
            "JSON positioned context has different provenance"
        );
        self.range(offset, len)?
            .with_positioned_view(context, Validation::Syntax, f)
    }
    /// Strict Value counterpart sharing an original-source positioned context across records.
    pub(crate) fn read_positioned_range_value<T>(
        &self,
        context: &RangeSource,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            context.matches_original(self),
            "JSON positioned context has different provenance"
        );
        self.range(offset, len)?
            .with_positioned_view(context, Validation::Value, f)
    }
    pub(crate) fn read_range_value<T>(
        &self,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.range(offset, len)?.with_validation(Validation::Value, f)
    }
    pub(crate) fn read_backing_value<T>(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.backing_range(backing, offset, len)?
            .with_validation(Validation::Value, f)
    }
    pub(crate) fn read_range_syntax<T>(
        &self,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.range(offset, len)?.with_validation(Validation::Syntax, f)
    }
    /// The producer captures/checks the original opened source before deriving these private bytes.
    /// Backing ownership and original provenance survive every selected child descriptor.
    pub(crate) fn read_backing_syntax<T>(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        offset: u64,
        len: u64,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.backing_range(backing, offset, len)?
            .with_validation(Validation::Syntax, f)
    }
    /// Only for store::text::Builder's successfully completed quoted JSON-string output.
    /// Its checked string/canonical writers guarantee strict UTF8/escape validity before this
    /// call; arbitrary decoded JSON and external source ranges must use validated readers.
    pub(crate) fn generated_string_text(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        offset: u64,
        len: u64,
        decoded_len: usize,
    ) -> Result<crate::lazy::Text> {
        let range = self.backing_range(backing, offset, len)?;
        if len > crate::lazy::INLINE_MAX as u64 {
            // The controlled producer has completed and strictly encoded this entire string.
            // Construct its descriptor without remapping/scanning the bytes it just emitted.
            range.check_retained_bounds()?;
            let mut text = crate::lazy::SourceText::new(Arc::new(range));
            text.known_len = Some(decoded_len);
            Ok(crate::lazy::Text::Source(text))
        } else {
            range.with_validation(Validation::Cached, |view| {
                view.source_text()?.context("constructed JSON string")
            })
        }
    }

    fn backing_range(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        offset: u64,
        len: u64,
    ) -> Result<JsonRange> {
        self.check()?;
        let backing_len = backing.len()?;
        self.decoded_range(backing, backing_len, offset, len)
    }
    fn decoded_range(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        backing_len: u64,
        offset: u64,
        len: u64,
    ) -> Result<JsonRange> {
        anyhow::ensure!(
            len != 0 && offset.checked_add(len).is_some_and(|end| end <= backing_len),
            "backing range out of bounds"
        );
        if backing.is_arena() {
            let mut decoded = self
                .0
                .decoded
                .lock()
                .map_err(|_| std::io::Error::other("decoded backing poisoned"))?;
            if decoded.is_none() {
                *decoded = Some(backing.clone());
            }
        }
        Ok(JsonRange {
            source: self.clone(),
            offset,
            len,
            decoded: Some(backing),
        })
    }
    pub(crate) fn read_document<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        self.read_range_value(0, self.0.identity.len, f)
    }
    /// Validate whole-source JSON grammar and UTF-8 without imposing Value's scalar/depth
    /// policy on ignored fields. Typed document consumers enforce their known-field schema
    /// and use accepted_array for records whose Value budget resets independently.
    pub(crate) fn read_syntax<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        if self.0.identity.len == 0 {
            return Err(InvalidJson.into());
        }
        self.read_range_syntax(0, self.0.identity.len, f)
    }
    /// Archive grammar and UTF-8 are whole-source; each child resets Value's depth budget.
    /// Invalid children remain None so a consumer can preserve first-record shape semantics.
    pub(crate) fn read_archive<T>(&self, f: impl for<'a> FnOnce(Option<AcceptedArray<'a>>) -> Result<T>) -> Result<T> {
        self.read_syntax(|view| f(view.accepted_array()?))
    }
    pub(crate) fn metadata_bytes(&self) -> usize {
        std::mem::size_of::<CapturedSource>() + 2 * std::mem::size_of::<usize>() + self.0.path.capacity()
    }
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self == other
    }
    /// Caller must have syntax AND Value-compatible scalar/depth validation for this complete
    /// range since capture. Quotes/containers are included; surrounding whitespace is excluded.
    /// Never use this for a decoded JSON string that represents a different IR argument value.
    pub(crate) fn validated_range(&self, offset: u64, len: u64) -> Result<Json> {
        self.check()?;
        Ok(Json::Source(Arc::new(self.range(offset, len)?)))
    }
}
#[cfg(all(test, feature = "mmap"))]
thread_local! {
    static NATIVE_VIEW_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl JsonRange {
    pub(crate) fn check(&self) -> Result<()> {
        self.source.check()?;
        Ok(())
    }

    pub(crate) fn check_retained_bounds(&self) -> Result<()> {
        self.check()?;
        if let Some(bytes) = &self.decoded {
            let len = bytes.len()?;
            anyhow::ensure!(
                self.offset.checked_add(self.len).is_some_and(|end| end <= len),
                "source text backing is truncated"
            );
        }
        self.check()
    }

    /// Captured auxiliary source dependencies, retaining original stamps and absence.
    /// Consumers merge these records into their cache guards; `path()` remains separate so
    /// append-aware consumers can handle their primary transcript identity independently.
    pub fn dependencies(&self) -> Option<&crate::source::ReadSet> {
        self.source.0.reads.as_deref()
    }
    pub(crate) fn same_range(&self, other: &Self) -> bool {
        self.source == other.source
            && self.offset == other.offset
            && self.len == other.len
            && match (&self.decoded, &other.decoded) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
    fn child(&self, offset: u64, len: u64) -> Result<Self> {
        anyhow::ensure!(
            len != 0
                && offset >= self.offset
                && offset.checked_add(len).is_some_and(|end| end <= self.offset + self.len),
            "JSON child out of bounds"
        );
        Ok(Self {
            source: self.source.clone(),
            offset,
            len,
            decoded: self.decoded.clone(),
        })
    }

    /// Reuse this range's original capture when building checked composite text. Recapturing
    /// its path would discard the identity/dependency boundary that accepted these bytes.
    pub(crate) fn captured_source(&self) -> &JsonSource {
        &self.source
    }

    /// Original transcript provenance; decoded argument ranges refer to private backing bytes.
    pub fn path(&self) -> &Path {
        &self.source.0.path
    }
    /// Offset in the backing document (which may be a decoded argument document).
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[cfg(feature = "mmap")]
    fn with_raw<T>(&self, f: impl for<'a> FnOnce(Raw<'a>) -> Result<T>) -> Result<T> {
        self.check()?;
        #[cfg(test)]
        NATIVE_VIEW_BYTES.with(|bytes| bytes.set(bytes.get() + self.len));
        let mut callback = Some(f);
        if let Some(bytes) = &self.decoded {
            if let Some(result) = bytes.with_bytes(|bytes| {
                let index = document::Index::default();
                let start = usize::try_from(self.offset)?;
                let end = start
                    .checked_add(usize::try_from(self.len)?)
                    .context("backing range overflow")?;
                let text = std::str::from_utf8(bytes.get(start..end).context("backing range out of bounds")?)
                    .map_err(|_| InvalidJson)?;
                callback.take().unwrap()(Raw::new(text, &index))
            })? {
                self.check()?;
                return result;
            }
        }
        let file = match &self.decoded {
            Some(bytes) => bytes.file()?,
            None => std::fs::File::open(self.path())?,
        };
        if self.decoded.is_none() {
            self.source.check_metadata(file.metadata()?)?;
        }
        let source = Source::map_file(&file)?;
        self.check()?;
        // Even validated ranges keep the safe UTF-8 check on newly mapped/read external bytes.
        // No unchecked string cast is justified by metadata identity.
        let result = source
            .slice(self.offset, self.len)
            .ok_or_else(|| InvalidJson.into())
            .and_then(callback.take().unwrap());
        // Check even after callback failure: an intentional staged-output cap must not hide
        // a source mutation. Source failure takes precedence over the callback error.
        if self.decoded.is_none() {
            self.source.check_metadata(file.metadata()?)?;
        }
        self.check()?;
        result
    }
    /// Bounded decoded demands must not map and UTF8-scan the entire retained string first.
    /// The string was strictly accepted at capture; positioned decoding checks demanded bytes,
    /// and original identity is checked even when the callback intentionally stops early.
    pub(crate) fn for_each_string_prefix_chunk(&self, visit: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        let source = match &self.decoded {
            Some(bytes) => RangeSource::decoded(self.source.clone(), bytes)?,
            None => RangeSource::from_captured(self.source.clone())?,
        };
        let range = Range {
            offset: self.offset,
            len: self.len,
        };
        let result = (|| {
            anyhow::ensure!(
                source.kind(range)? == Some(b'"'),
                "source text backing is not a JSON string"
            );
            source.for_each_string_chunk_until(range, visit)
        })();
        source.check_identity()?;
        self.check()?;
        result
    }

    /// Prefix consumers reuse the checked positioned writer in both feature configurations.
    /// Capture already accepted the complete range; demanded chunks retain safe UTF-8 decoding,
    /// and descriptor/dependency checks still run after an intentional output-budget stop.
    fn with_bounded_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        let source = match &self.decoded {
            Some(bytes) => RangeSource::decoded(self.source.clone(), bytes)?,
            None => RangeSource::from_captured(self.source.clone())?,
        };
        self.with_positioned_view(&source, Validation::Cached, f)
    }

    pub(crate) fn with_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        self.with_validation(Validation::Cached, f)
    }
    fn with_native_view<T>(
        &self,
        raw: Raw<'_>,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let base = raw.get().as_ptr() as usize;
        let index = document::Index::default();
        let raw = match validation {
            Validation::Cached => raw,
            // One pass accepting exactly Value's language, indexing long fields as it goes.
            Validation::Value => {
                let checked = Raw::new(raw.get().trim_matches(&[' ', '\n', '\t', '\r'][..]), &index);
                if !document::validate(checked) {
                    return Err(InvalidJson.into());
                }
                checked
            }
            Validation::Syntax => {
                let syntax: &crate::lazy::RawValue = serde_json::from_str(raw.get()).map_err(|_| InvalidJson)?;
                Raw::new(syntax.get(), &index)
            }
        };
        f(JsonView(View::Source(SourceView { raw, range: self, base })))
    }
    fn with_validation<T>(
        &self,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        #[cfg(feature = "mmap")]
        {
            self.with_raw(|raw| self.with_native_view(raw, validation, f))
        }
        #[cfg(not(feature = "mmap"))]
        {
            let source = match &self.decoded {
                Some(bytes) => RangeSource::decoded(self.source.clone(), bytes)?,
                None => RangeSource::from_captured(self.source.clone())?,
            };
            self.with_positioned_view(&source, validation, f)
        }
    }
    fn with_positioned_view<T>(
        &self,
        source: &RangeSource,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let requested = Range {
            offset: self.offset,
            len: self.len,
        };
        let result = (|| {
            let value = match validation {
                Validation::Cached => requested,
                Validation::Value | Validation::Syntax => {
                    let valid = if matches!(validation, Validation::Value) {
                        source.validate(requested)?
                    } else {
                        source.syntax(requested)?
                    };
                    if !valid {
                        return Err(InvalidJson.into());
                    }
                    source.cursor(requested)?.value()?
                }
            };
            f(JsonView(View::Portable(PortableView {
                source,
                owner: self,
                value,
            })))
        })();
        source.check_identity()?;
        self.check()?;
        result
    }
}

#[derive(Clone, Copy)]
enum Validation {
    Cached,
    Value,
    Syntax,
}

pub(crate) struct AcceptedArray<'a>(JsonArray<'a>);
impl<'a> Iterator for AcceptedArray<'a> {
    type Item = Result<Option<JsonView<'a>>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|value| {
            let value = value?;
            Ok(value.accepts_value()?.then_some(value))
        })
    }
}

impl From<Value> for Json {
    fn from(value: Value) -> Self {
        Self::Owned(value)
    }
}
impl Default for Json {
    fn default() -> Self {
        Self::Owned(Value::Null)
    }
}
impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.resolve().map_err(serde::ser::Error::custom)?.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::Owned)
    }
}
impl Json {
    /// Validate a complete JSON range before constructing a source descriptor. Missing files,
    /// overflow, truncated ranges and values rejected by serde_json Value return errors.
    pub fn source(path: impl AsRef<Path>, offset: u64, len: u64) -> Result<Self> {
        Self::source_with_reads(path, offset, len, &crate::source::ReadSet::default())
    }
    /// Validate a complete JSON range under an already captured auxiliary dependency guard.
    /// Guarded dependencies (including missing conditional files) must still be fresh.
    pub fn source_with_reads(
        path: impl AsRef<Path>,
        offset: u64,
        len: u64,
        reads: &crate::source::ReadSet,
    ) -> Result<Self> {
        let source = JsonSource::capture_with_reads(path.as_ref(), reads)?;
        let mut range = source.range(offset, len)?;
        #[cfg(feature = "mmap")]
        let (leading, normalized_len) = range.with_raw(|raw| {
            // RawValue validates syntax; the shared strict preflight supplies Value's finite
            // numbers, UTF-16 and recursion budget without decoding descendant strings.
            let syntax: &crate::lazy::RawValue = serde_json::from_str(raw.get()).context("invalid JSON range")?;
            let index = document::Index::default();
            let checked = Raw::new(syntax.get(), &index);
            anyhow::ensure!(
                document::accepts_value(checked),
                "JSON range is not a valid serde_json Value"
            );
            Ok((
                (syntax.get().as_ptr() as usize - raw.get().as_ptr() as usize) as u64,
                syntax.get().len() as u64,
            ))
        })?;
        #[cfg(not(feature = "mmap"))]
        let (leading, normalized_len) = {
            let context = RangeSource::from_captured(source.clone())?;
            let requested = Range { offset, len };
            anyhow::ensure!(
                context.validate(requested)?,
                "JSON range is not a valid serde_json Value"
            );
            let normalized = context.cursor(requested)?.value()?;
            context.check_identity()?;
            (normalized.offset - offset, normalized.len)
        };
        range.offset += leading;
        range.len = normalized_len;
        range.source.validated_range(range.offset, range.len)
    }
    /// Select a child value without materializing its descendants. Source children retain their
    /// validated range and source identity; missing keys/nonobjects remain distinct from errors.
    pub fn select(&self, key: &str) -> Result<Option<Self>> {
        self.with_view(|view| view.get(key)?.map(JsonView::to_json).transpose())
    }

    /// Borrow owned data or materialize the selected complete source value. This is deliberately
    /// explicit: there is no implicit Deref that builds a giant tree or substitutes missing data.
    pub fn resolve(&self) -> Result<Cow<'_, Value>> {
        match self {
            Self::Owned(value) => Ok(Cow::Borrowed(value)),
            Self::Source(range) => range.with_view(|view| Ok(Cow::Owned(view.resolve()?.into_owned()))),
        }
    }
    /// Resolve before mutation. A failed read leaves the source descriptor intact.
    pub fn make_owned(&mut self) -> Result<&mut Value> {
        if let Self::Source(_) = self {
            *self = Self::Owned(self.resolve()?.into_owned());
        }
        match self {
            Self::Owned(value) => Ok(value),
            Self::Source(_) => unreachable!(),
        }
    }
    /// A scoped view cannot outlive its backing mapping/buffer. All source read/validation errors
    /// are distinct from a missing field; a changed source also rejects the completed operation.
    pub fn with_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        match self {
            Self::Owned(value) => f(JsonView(View::Owned(value))),
            Self::Source(range) => range.with_view(f),
        }
    }
    /// Byte length of Value's compact serialization, preserving insertion order and duplicates'
    /// final values at their first key position. Does not materialize descendant strings.
    pub fn canonical_len(&self) -> Result<u64> {
        let mut counter = CountWriter(0);
        self.write_canonical(&mut counter)?;
        Ok(counter.0)
    }
    /// A compact JSON byte prefix ending on a Unicode scalar boundary, plus whether the complete
    /// value was truncated. Source validation and the final identity check still apply. No more
    /// than max_bytes of output are retained; the prefix need not itself be complete JSON.
    pub fn canonical_prefix(&self, max_bytes: usize) -> Result<(String, bool)> {
        match self {
            Self::Owned(value) => JsonView::from(value).canonical_prefix(max_bytes),
            Self::Source(range) => range.with_bounded_view(|view| view.canonical_prefix(max_bytes)),
        }
    }
    /// Emit the exact compact JSON that serde_json serializes from the complete Value.
    ///
    /// An I/O or source-change error may occur after a prefix has been written. Callers must
    /// discard/reset staged output on Err and must not publish consumer freshness for that pass.
    /// Generic Serialize instead resolves the selected value, as serializers cannot stream pieces
    /// of one string. This explicit writer keeps decoded leaf scratch bounded.
    pub fn write_canonical(&self, writer: &mut impl Write) -> Result<()> {
        self.with_view(|view| view.write_canonical(writer))
    }
}

#[derive(Clone, Copy)]
enum View<'a> {
    Owned(&'a Value),
    Source(SourceView<'a>),
    Portable(PortableView<'a>),
}
#[derive(Clone, Copy)]
struct SourceView<'a> {
    raw: Raw<'a>,
    range: &'a JsonRange,
    base: usize,
}
#[derive(Clone, Copy)]
struct PortableView<'a> {
    source: &'a RangeSource,
    owner: &'a JsonRange,
    value: Range,
}
/// A typed projection of an already validated complete JSON value, scoped by Json::with_view.
#[derive(Clone, Copy)]
pub struct JsonView<'a>(View<'a>);
impl<'a> From<&'a Value> for JsonView<'a> {
    fn from(value: &'a Value) -> Self {
        Self(View::Owned(value))
    }
}
fn raw_string_may_contain_ascii(raw: &[u8], byte: u8) -> bool {
    if memchr::memchr(byte, raw).is_some() {
        return true;
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut escape = [
        b'\\',
        b'u',
        b'0',
        b'0',
        HEX[(byte >> 4) as usize],
        HEX[(byte & 15) as usize],
    ];
    if memchr::memmem::find(raw, &escape).is_some() {
        return true;
    }
    escape[5].make_ascii_uppercase();
    memchr::memmem::find(raw, &escape).is_some()
}
impl<'a> JsonView<'a> {
    #[cfg(any(feature = "sqlite", test))]
    pub(crate) fn owned(value: &'a Value) -> Self {
        Self(View::Owned(value))
    }

    /// Child records retain their original independent Value-compatible acceptance boundary.
    pub(crate) fn accepted_array(self) -> Result<Option<AcceptedArray<'a>>> {
        Ok(self.array()?.map(AcceptedArray))
    }
    pub(crate) fn accepts_value(self) -> Result<bool> {
        self.accepts_value_at_depth(0)
    }
    pub(crate) fn accepts_value_at_depth(self, depth: usize) -> Result<bool> {
        match self.0 {
            View::Owned(_) => Ok(true),
            View::Source(source) => Ok(document::accepts_value_at_depth(source.raw, depth)),
            View::Portable(source) => source.source.accepts_value_at_depth(source.value, depth),
        }
    }
    pub(crate) fn select_fields(self, keys: &[&str]) -> Result<Option<Vec<Option<Self>>>> {
        self.select_fields_with(keys, false)
    }
    /// Visit requested fields; the last visit of a key wins. False for a nonobject.
    pub(crate) fn select_each(
        self,
        keys: &[&str],
        unique: bool,
        visit: &mut dyn FnMut(usize, Self) -> Result<()>,
    ) -> Result<bool> {
        if let View::Source(source) = self.0 {
            return document::select_each(source.raw, keys, unique, &mut |i, raw| {
                visit(i, Self(View::Source(SourceView { raw, ..source })))
            });
        }
        let Some(fields) = self.select_fields_with(keys, unique)? else {
            return Ok(false);
        };
        for (i, field) in fields.into_iter().enumerate() {
            if let Some(field) = field {
                visit(i, field)?;
            }
        }
        Ok(true)
    }
    fn select_fields_with(self, keys: &[&str], unique: bool) -> Result<Option<Vec<Option<Self>>>> {
        Ok(match self.0 {
            View::Owned(Value::Object(object)) => Some(
                keys.iter()
                    .map(|key| object.get(*key).map(|value| Self(View::Owned(value))))
                    .collect(),
            ),
            View::Owned(_) => None,
            View::Source(source) => (if unique {
                document::select_unique_fields(source.raw, keys)?
            } else {
                document::select_fields(source.raw, keys)
            })
            .map(|fields| {
                fields
                    .into_iter()
                    .map(|raw| raw.map(|raw| Self(View::Source(SourceView { raw, ..source }))))
                    .collect()
            }),
            View::Portable(source) => (if unique {
                source.source.select_unique_fields(source.value, keys)?
            } else {
                source.source.select_fields(source.value, keys)?
            })
            .map(|fields| {
                fields
                    .into_iter()
                    .map(|value| value.map(|value| Self(View::Portable(PortableView { value, ..source }))))
                    .collect()
            }),
        })
    }
    pub(crate) fn source_range(self) -> Result<Option<(u64, u64)>> {
        Ok(match self.0 {
            View::Owned(_) => None,
            View::Source(source) => {
                let delta = (source.raw.get().as_ptr() as usize)
                    .checked_sub(source.base)
                    .context("JSON child outside source")? as u64;
                Some((
                    source.range.offset.checked_add(delta).context("JSON offset overflow")?,
                    source.raw.get().len() as u64,
                ))
            }
            View::Portable(source) => Some((source.value.offset, source.value.len)),
        })
    }
    /// Retain the selected string independently of a session's implicit primary path.
    pub fn source_text(self) -> Result<Option<crate::lazy::Text>> {
        if !self.is_string()? {
            return Ok(None);
        }
        if self
            .source_range()?
            .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            if let Json::Source(range) = self.to_json()? {
                return Ok(Some(crate::lazy::Text::Source(crate::lazy::SourceText::new(range))));
            }
        }
        self.text(false)
    }
    /// Transcript text by the codec's span rule: a string whose *decoded* length exceeds
    /// INLINE_MAX stays in the source when `spans` is demanded, and anything else is inline.
    /// Both builds describe it as a [`Span`](crate::lazy::Span) of the session's primary file,
    /// held to the session's parse-time stamp (same file, only grown). None for a nonstring.
    pub(crate) fn lazy_text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        if !self.is_string()? {
            return Ok(None);
        }
        self.text(spans && self.exceeds_inline()?)
    }
    /// [`lazy_text`](Self::lazy_text) for a store whose strings live in many files or cells:
    /// text that stays in the source is a checked source text naming its own bytes, never a span
    /// of the session's primary file.
    pub(crate) fn lazy_source_text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        if !self.is_string()? {
            return Ok(None);
        }
        if spans && self.exceeds_inline()? {
            return self.source_text();
        }
        self.text(false)
    }
    /// The span rule: a string whose decoded length exceeds INLINE_MAX.
    fn exceeds_inline(self) -> Result<bool> {
        let Some((_, len)) = self.source_range()? else {
            return Ok(false);
        };
        if len - 2 <= crate::lazy::INLINE_MAX as u64 {
            return Ok(false);
        }
        // A JSON escape is at most six bytes per decoded byte: only near-threshold escaped
        // strings need their decoded length, and those are at most 24 KiB of input.
        if len - 2 > 6 * crate::lazy::INLINE_MAX as u64 {
            return Ok(true);
        }
        let mut decoded = 0;
        self.for_each_char(|c| {
            decoded += c.len_utf8();
            decoded <= crate::lazy::INLINE_MAX
        })?;
        Ok(decoded > crate::lazy::INLINE_MAX)
    }
    /// Visit a string's decoded characters until `visit` returns false. A borrowed string is
    /// decoded in place, without a decode window; false for a nonstring.
    pub(crate) fn for_each_char(self, mut visit: impl FnMut(char) -> bool) -> Result<bool> {
        if let View::Source(source) = self.0 {
            let raw = source.raw.get();
            if !raw.starts_with('"') {
                return Ok(false);
            }
            let mut chars = raw[1..raw.len() - 1].chars();
            while let Some(c) = chars.next() {
                let c = if c != '\\' {
                    c
                } else {
                    // Validated: every escape is well formed and surrogates are paired.
                    let hex = |chars: &mut std::str::Chars<'_>| {
                        (0..4).try_fold(0, |n, _| Some(n * 16 + chars.next()?.to_digit(16)?))
                    };
                    match chars.next() {
                        Some('b') => '\u{8}',
                        Some('f') => '\u{c}',
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('u') => {
                            let high = hex(&mut chars).context("validated JSON escape")?;
                            let code = if (0xd800..=0xdbff).contains(&high) {
                                chars.nth(1);
                                let low = hex(&mut chars).context("validated JSON escape")?;
                                0x10000 + ((high - 0xd800) << 10) + low - 0xdc00
                            } else {
                                high
                            };
                            char::from_u32(code).context("validated JSON escape")?
                        }
                        Some(other) => other,
                        None => anyhow::bail!("validated JSON string failed decoding"),
                    }
                };
                if !visit(c) {
                    break;
                }
            }
            return Ok(true);
        }
        #[derive(Debug)]
        struct Stop;
        impl std::fmt::Display for Stop {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("character visit stopped")
            }
        }
        impl std::error::Error for Stop {}
        match self.for_each_str_chunk(|chunk| {
            for c in chunk.chars() {
                if !visit(c) {
                    return Err(Stop.into());
                }
            }
            Ok(())
        }) {
            Err(error) if error.is::<Stop>() => Ok(true),
            result => result,
        }
    }
    /// Native transcript text uses the session's primary source, just like existing Text spans.
    pub(crate) fn text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        if spans
            && match self.0 {
                View::Source(source) => source.range.decoded.is_some(),
                View::Portable(source) => source.owner.decoded.is_some(),
                View::Owned(_) => false,
            }
        {
            return self.source_text();
        }
        match self.0 {
            View::Owned(value) => Ok(value.as_str().map(|s| crate::lazy::Text::Inline(s.to_owned()))),
            View::Source(source) => {
                let raw = source.raw.get().as_bytes();
                if raw.first() != Some(&b'"') {
                    return Ok(None);
                }
                if spans && source.range.decoded.is_none() && raw.len() - 2 > crate::lazy::INLINE_MAX {
                    let (offset, len) = self.source_range()?.context("JSON text source")?;
                    Ok(Some(crate::lazy::Text::Span(crate::lazy::Span {
                        source: None,
                        offset: offset + 1,
                        len: len - 2,
                        escaped: memchr::memchr(b'\\', &raw[1..raw.len() - 1]).is_some(),
                    })))
                } else {
                    Ok(Some(crate::lazy::Text::Inline(serde_json::from_slice(raw)?)))
                }
            }
            View::Portable(source) => source
                .source
                .text(source.value, spans && source.owner.decoded.is_none()),
        }
    }
    pub(crate) fn argument(self, spans: bool) -> Result<Json> {
        if spans
            && self
                .source_range()?
                .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            self.to_json()
        } else {
            Ok(Json::Owned(self.resolve()?.into_owned()))
        }
    }
    /// Decode OpenAI's encoded argument document into a private bounded backing. Invalid,
    /// partial and scalar arguments retain the original string representation.
    pub(crate) fn encoded_argument(self) -> Result<Json> {
        if self.is_null()? {
            return Ok(serde_json::json!({}).into());
        }
        if !self.is_string()? {
            return self.to_json();
        }
        if !self
            .source_range()?
            .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            return Ok(crate::harness::wire::openai::parse_arguments(Some(self.resolve()?.as_ref())).into());
        }
        let original = self.to_json()?;
        let Json::Source(range) = &original else {
            return Ok(crate::harness::wire::openai::parse_arguments(Some(self.resolve()?.as_ref())).into());
        };
        let bytes = range.source.decoded_backing()?;
        let mut blank = true;
        let (offset, len) = bytes.append(|writer| {
            self.for_each_str_chunk(|part| {
                blank &= part.trim().is_empty();
                writer.write_all(part.as_bytes())?;
                Ok(())
            })?;
            Ok(())
        })?;
        if blank {
            return Ok(serde_json::json!({}).into());
        }
        match range.source.read_backing_value(bytes, offset, len, |view| {
            if view.is_object()? || view.is_array()? {
                Ok(Some(view.to_json()?))
            } else {
                Ok(None)
            }
        }) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Ok(original),
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => Ok(original),
            Err(error) => Err(error),
        }
    }
    /// A nonobject or missing field is None; backing-source failures occur before this view exists.
    pub fn get(self, key: &str) -> Result<Option<Self>> {
        Ok(match self.0 {
            View::Owned(value) => value.get(key).map(|value| Self(View::Owned(value))),
            View::Portable(source) => source
                .source
                .get(source.value, key)?
                .map(|value| Self(View::Portable(PortableView { value, ..source }))),
            View::Source(source) => document::select_fields(source.raw, &[key])
                .and_then(|mut fields| fields.pop().flatten())
                .map(|raw| Self(View::Source(SourceView { raw, ..source }))),
        })
    }
    /// Conservative printable-ASCII presence without decoding a retained JSON string.
    /// False proves absence; escaped backslashes may create harmless false positives.
    pub(crate) fn string_may_contain_ascii(self, byte: u8) -> Result<bool> {
        anyhow::ensure!((0x20..=0x7e).contains(&byte), "expected printable ASCII");
        match self.0 {
            View::Owned(value) => Ok(value.as_str().is_some_and(|s| s.as_bytes().contains(&byte))),
            View::Source(source) => Ok(
                source.raw.get().starts_with('"') && raw_string_may_contain_ascii(source.raw.get().as_bytes(), byte)
            ),
            View::Portable(source) => {
                use std::io::Read;
                if source.source.kind(source.value)? != Some(b'"') {
                    return Ok(false);
                }
                let mut cursor = source.source.cursor(source.value)?;
                let mut buffer = [0u8; 64 * 1024 + 5];
                let mut overlap = 0;
                loop {
                    let n = cursor.read(&mut buffer[overlap..])?;
                    if n == 0 {
                        return Ok(false);
                    }
                    let end = overlap + n;
                    if raw_string_may_contain_ascii(&buffer[..end], byte) {
                        return Ok(true);
                    }
                    overlap = end.min(5);
                    buffer.copy_within(end - overlap..end, 0);
                }
            }
        }
    }
    pub fn is_string(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_string(),
            View::Portable(source) => source.source.kind(source.value)? == Some(b'"'),
            View::Source(source) => source.raw.get().starts_with('"'),
        })
    }
    pub fn is_object(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_object(),
            View::Portable(source) => source.source.kind(source.value)? == Some(b'{'),
            View::Source(source) => source.raw.get().starts_with('{'),
        })
    }
    pub fn is_array(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_array(),
            View::Portable(source) => source.source.kind(source.value)? == Some(b'['),
            View::Source(source) => source.raw.get().starts_with('['),
        })
    }
    pub fn is_null(self) -> Result<bool> {
        Ok(match self.0 {
            View::Owned(value) => value.is_null(),
            View::Portable(source) => source.source.kind(source.value)? == Some(b'n'),
            View::Source(source) => source.raw.get() == "null",
        })
    }
    pub fn as_bool(self) -> Result<Option<bool>> {
        Ok(match self.0 {
            View::Owned(value) => value.as_bool(),
            View::Portable(source) => match source.source.kind(source.value)? {
                Some(b't') => Some(true),
                Some(b'f') => Some(false),
                _ => None,
            },
            View::Source(source) => match source.raw.get() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
        })
    }
    pub fn as_number(self) -> Result<Option<Number>> {
        match self.0 {
            View::Owned(value) => Ok(value.as_number().cloned()),
            View::Source(source) if matches!(source.raw.get().as_bytes().first(), Some(b'-' | b'0'..=b'9')) => {
                Ok(Some(serde_json::from_str(source.raw.get())?))
            }
            View::Source(_) => Ok(None),
            View::Portable(source) => {
                if matches!(source.source.kind(source.value)?, Some(b'-' | b'0'..=b'9')) {
                    Ok(Some(source.source.materialize(source.value)?))
                } else {
                    Ok(None)
                }
            }
        }
    }
    /// Chronological array traversal holds one descriptor, not a vector of all children.
    pub fn array(self) -> Result<Option<JsonArray<'a>>> {
        Ok(match self.0 {
            View::Owned(value) => value.as_array().map(|array| JsonArray(Array::Owned(array.iter()))),
            View::Portable(source) => source
                .source
                .array(source.value)?
                .map(|array| JsonArray(Array::Portable { array, source })),
            View::Source(source) => {
                document::array_iter(source.raw).map(|array| JsonArray(Array::Source { array, source }))
            }
        })
    }
    /// Visit normalized object entries in Value's insertion order. Structural key state grows
    /// with object width; descendant payloads are borrowed, not decoded into a tree.
    pub fn for_each_entry(self, mut f: impl FnMut(&str, Self) -> Result<()>) -> Result<bool> {
        match self.0 {
            View::Owned(Value::Object(object)) => {
                for (key, value) in object {
                    f(key, Self(View::Owned(value)))?;
                }
            }
            View::Source(source) => {
                let Some(entries) = document::object_entries(source.raw) else {
                    return Ok(false);
                };
                for (key, value) in entries {
                    f(&key, Self(View::Source(SourceView { raw: value, ..source })))?;
                }
            }
            View::Portable(source) => {
                let Some(entries) = source.source.object(source.value)? else {
                    return Ok(false);
                };
                let mut fields = std::collections::BTreeMap::<String, (usize, Range)>::new();
                for (i, entry) in entries.enumerate() {
                    let (key, value) = entry?;
                    let key: String = source.source.materialize(key)?;
                    fields
                        .entry(key)
                        .and_modify(|(_, old)| *old = value)
                        .or_insert((i, value));
                }
                let mut ordered = fields.iter().collect::<Vec<_>>();
                ordered.sort_by_key(|(_, (position, _))| *position);
                for (key, (_, value)) in ordered {
                    f(
                        key,
                        Self(View::Portable(PortableView {
                            value: *value,
                            ..source
                        })),
                    )?;
                }
            }
            View::Owned(_) => return Ok(false),
        }
        Ok(true)
    }
    /// Visit decoded string fragments with bounded source scratch. Returns false for a nonstring.
    /// Errors are explicit; callback effects must be discarded if this operation returns Err.
    pub fn for_each_str_chunk(self, mut f: impl FnMut(&str) -> Result<()>) -> Result<bool> {
        match self.0 {
            View::Owned(value) => match value.as_str() {
                Some(text) => {
                    f(text)?;
                    Ok(true)
                }
                None => Ok(false),
            },
            View::Source(source) if source.raw.get().starts_with('"') => {
                decode_string(source.raw.get(), &mut f)?;
                Ok(true)
            }
            View::Source(_) => Ok(false),
            View::Portable(source) => {
                if source.source.kind(source.value)? != Some(b'"') {
                    return Ok(false);
                };
                source.source.for_each_string_chunk_until(source.value, &mut f)?;
                Ok(true)
            }
        }
    }
    /// Own a descriptor for this complete child. Source values copy only range metadata, while
    /// already owned values explicitly clone their selected data.
    pub fn to_json(self) -> Result<Json> {
        match self.0 {
            View::Owned(value) => Ok(Json::Owned(value.clone())),
            View::Portable(source) => Ok(Json::Source(Arc::new(
                source.owner.child(source.value.offset, source.value.len)?,
            ))),
            View::Source(source) => {
                let delta = (source.raw.get().as_ptr() as usize)
                    .checked_sub(source.base)
                    .context("JSON child outside source")? as u64;
                let offset = source
                    .range
                    .offset
                    .checked_add(delta)
                    .context("JSON child offset overflow")?;
                // The containing scoped operation performs the final identity check; child
                // descriptors do not need another stat for each field within that operation.
                Ok(Json::Source(Arc::new(
                    source.range.child(offset, source.raw.get().len() as u64)?,
                )))
            }
        }
    }
    /// Materialize only this selected view. Source strings/containers may allocate their full data.
    pub fn resolve(self) -> Result<Cow<'a, Value>> {
        match self.0 {
            View::Owned(value) => Ok(Cow::Borrowed(value)),
            View::Source(source) => Ok(Cow::Owned(serde_json::from_str(source.raw.get())?)),
            View::Portable(source) => Ok(Cow::Owned(source.source.materialize(source.value)?)),
        }
    }
    pub fn canonical_len(self) -> Result<u64> {
        let mut counter = CountWriter(0);
        self.write_canonical(&mut counter)?;
        Ok(counter.0)
    }
    /// Same bounded output and truncation contract as Json::canonical_prefix.
    pub fn canonical_prefix(self, max_bytes: usize) -> Result<(String, bool)> {
        self.formatted_prefix(max_bytes, false)
    }
    /// A bounded Unicode-complete prefix of serde_json's default pretty representation.
    /// The containing scoped view retains its normal final source-identity check.
    pub fn pretty_prefix(self, max_bytes: usize) -> Result<(String, bool)> {
        self.formatted_prefix(max_bytes, true)
    }
    fn formatted_prefix(self, max_bytes: usize, pretty: bool) -> Result<(String, bool)> {
        let mut writer = PrefixWriter {
            bytes: Vec::new(),
            limit: max_bytes,
            stopped: false,
        };
        let result = if pretty {
            self.write_canonical(&mut crate::PrettyJson::new(&mut writer, 0))
        } else {
            self.write_canonical(&mut writer)
        };
        if !writer.stopped {
            result?;
        }
        Ok((String::from_utf8(writer.bytes)?, writer.stopped))
    }
    /// Canonicalize with source-identity-specific replacements selected from this capture.
    /// Replacements never change acceptance: the producer must validate the complete original.
    /// The replacement [`Self::write_canonical_replacing`] would write for this exact node.
    pub(crate) fn replacement(self, replacements: &[(Json, Value)]) -> Result<Option<&Value>> {
        let owner = match self.0 {
            View::Owned(_) => None,
            View::Source(source) => Some(source.range),
            View::Portable(source) => Some(source.owner),
        };
        if let Some(owner) = owner {
            let span = self.source_range()?;
            for (original, replacement) in replacements {
                if let Json::Source(range) = original {
                    let same_backing = match (&range.decoded, &owner.decoded) {
                        (None, None) => true,
                        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                        _ => false,
                    };
                    if Arc::ptr_eq(&range.source.0, &owner.source.0)
                        && same_backing
                        && span == Some((range.offset, range.len))
                    {
                        return Ok(Some(replacement));
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn write_canonical_replacing(
        self,
        replacements: &[(Json, Value)],
        writer: &mut impl Write,
    ) -> Result<()> {
        if let Some(replacement) = self.replacement(replacements)? {
            serde_json::to_writer(writer, replacement)?;
            return Ok(());
        }
        if self.is_object()? {
            writer.write_all(b"{")?;
            let mut first = true;
            self.for_each_entry(|key, child| {
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                child.write_canonical_replacing(replacements, writer)
            })?;
            writer.write_all(b"}")?;
        } else if let Some(array) = self.array()? {
            writer.write_all(b"[")?;
            for (index, child) in array.enumerate() {
                if index != 0 {
                    writer.write_all(b",")?;
                }
                child?.write_canonical_replacing(replacements, writer)?;
            }
            writer.write_all(b"]")?;
        } else {
            self.write_canonical(writer)?;
        }
        Ok(())
    }

    /// Same partial-output-on-error contract as Json::write_canonical.
    pub fn write_canonical(self, writer: &mut impl Write) -> Result<()> {
        match self.0 {
            View::Owned(value) => serde_json::to_writer(writer, value).map_err(Into::into),
            View::Source(_) | View::Portable(_) => write_source(self, writer),
        }
    }
}

enum Array<'a> {
    Portable {
        array: document::range::ArrayRanges<'a>,
        source: PortableView<'a>,
    },
    Owned(std::slice::Iter<'a, Value>),
    Source {
        array: document::ArrayIter<'a>,
        source: SourceView<'a>,
    },
}
/// A scoped chronological array iterator. No source payloads or child roster are owned.
/// Items are fallible so file-backed cursor implementations can propagate read errors.
pub struct JsonArray<'a>(Array<'a>);
impl<'a> Iterator for JsonArray<'a> {
    type Item = Result<JsonView<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Array::Owned(array) => array.next().map(|value| Ok(JsonView(View::Owned(value)))),
            Array::Portable { array, source } => array.next().map(|value| {
                value
                    .map(|value| JsonView(View::Portable(PortableView { value, ..*source })))
                    .map_err(Into::into)
            }),
            Array::Source { array, source } => array
                .next()
                .map(|raw| Ok(JsonView(View::Source(SourceView { raw, ..*source })))),
        }
    }
}

struct PrefixWriter {
    bytes: Vec<u8>,
    limit: usize,
    stopped: bool,
}
impl Write for PrefixWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let remaining = self.limit - self.bytes.len();
        let mut end = bytes.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let needed = self.bytes.len() + end;
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .max(32)
                .saturating_mul(2)
                .min(self.limit)
                .max(needed);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(&bytes[..end]);
        if end != bytes.len() {
            self.stopped = true;
            return Err(std::io::Error::other("canonical JSON prefix complete"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct CountWriter(u64);
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("JSON length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Decode a validated JSON string (quotes included): unescaped text is borrowed whole, escaped
/// text goes through the one body codec in bounded windows.
fn decode_string(quoted: &str, f: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    let body = &quoted[1..quoted.len() - 1];
    if !body.as_bytes().contains(&b'\\') {
        return f(body);
    }
    let mut error = None;
    let decoded = crate::harness::store::json::decode_body(&mut body.as_bytes(), &mut |text| {
        error = f(text).err();
        error.is_none()
    });
    if let Some(error) = error {
        return Err(error);
    }
    anyhow::ensure!(decoded.is_ok(), "validated JSON string failed decoding");
    Ok(())
}
/// One container walk for both byte sources; only scalar access depends on the backing.
fn write_source(value: JsonView<'_>, writer: &mut impl Write) -> Result<()> {
    let kind = match value.0 {
        View::Source(source) => source.raw.get().as_bytes().first().copied(),
        View::Portable(source) => source.source.kind(source.value)?,
        View::Owned(_) => unreachable!(),
    };
    match kind {
        Some(b'{') => {
            writer.write_all(b"{")?;
            let mut first = true;
            value.for_each_entry(|key, child| {
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                child.write_canonical(writer)
            })?;
            writer.write_all(b"}")?;
        }
        Some(b'[') => {
            writer.write_all(b"[")?;
            for (i, child) in value.array()?.context("invalid validated JSON array")?.enumerate() {
                if i != 0 {
                    writer.write_all(b",")?;
                }
                child?.write_canonical(writer)?;
            }
            writer.write_all(b"]")?;
        }
        Some(b'"') => {
            if let View::Source(source) = value.0 {
                // Preserve the mapped fast path: only Unicode and optional slash escapes need
                // decoding to match serde's canonical spelling. Accepted bytes can pass through.
                let bytes = source.raw.get().as_bytes();
                if !bytes.contains(&b'\\')
                    || (memchr::memmem::find(bytes, b"\\u").is_none() && memchr::memmem::find(bytes, b"\\/").is_none())
                {
                    writer.write_all(bytes)?;
                    return Ok(());
                }
            }
            writer.write_all(b"\"")?;
            let mut scratch = Vec::new();
            let mut encode = |text: &str| {
                scratch.clear();
                serde_json::to_writer(&mut scratch, text)?;
                writer.write_all(&scratch[1..scratch.len() - 1])?;
                Ok(())
            };
            match value.0 {
                View::Source(source) => decode_string(source.raw.get(), &mut encode)?,
                View::Portable(source) => source.source.for_each_string_chunk_until(source.value, &mut encode)?,
                View::Owned(_) => unreachable!(),
            }
            writer.write_all(b"\"")?;
        }
        Some(b'-' | b'0'..=b'9') => {
            let number: Number = match value.0 {
                View::Source(source) => serde_json::from_str(source.raw.get())?,
                View::Portable(source) => source.source.materialize(source.value)?,
                View::Owned(_) => unreachable!(),
            };
            serde_json::to_writer(writer, &number)?;
        }
        Some(b't') => writer.write_all(b"true")?,
        Some(b'f') => writer.write_all(b"false")?,
        Some(b'n') => writer.write_all(b"null")?,
        _ => anyhow::bail!("invalid validated JSON scalar"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn canonical_replacements_require_same_capture_and_exact_range() {
        let world = cv_testkit::World::new("json-canonical-replacements");
        let raw = r#"{"z":"first","a":{"content":"old","content":"final"},"z":"last","n":1.50}"#;
        let left = world.base.join("left.json");
        let right = world.base.join("right.json");
        std::fs::write(&left, raw).unwrap();
        std::fs::write(&right, raw).unwrap();
        let left = Json::source(&left, 0, raw.len() as u64).unwrap();
        let right = Json::source(&right, 0, raw.len() as u64).unwrap();
        let selected = left.select("a").unwrap().unwrap().select("content").unwrap().unwrap();
        let replacements = vec![(selected, Value::String("replacement".into()))];
        let mut actual = Vec::new();
        left.with_view(|view| view.write_canonical_replacing(&replacements, &mut actual))
            .unwrap();
        let mut expected: Value = serde_json::from_str(raw).unwrap();
        expected["a"]["content"] = Value::String("replacement".into());
        assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
        actual.clear();
        right
            .with_view(|view| view.write_canonical_replacing(&replacements, &mut actual))
            .unwrap();
        assert_eq!(
            actual,
            serde_json::to_vec(&serde_json::from_str::<Value>(raw).unwrap()).unwrap()
        );
    }
    use super::*;
    use serde_json::json;

    #[cfg(not(feature = "mmap"))]
    #[test]
    fn positioned_syntax_context_preserves_capture_and_failure_precedence() {
        let first = Fixture::new(br#"{"known":1,"ignored":"\ud800"}"#);
        let second = Fixture::new(br#"{"known":1,"ignored":"\ud800"}"#);
        let source = JsonSource::capture(&first.path).unwrap();
        let other = JsonSource::capture(&second.path).unwrap();
        let context = RangeSource::from_captured(source.clone()).unwrap();
        assert_eq!(
            source
                .read_positioned_range_syntax(&context, 0, source.len(), |view| {
                    Ok(view.get("known")?.unwrap().resolve()?.as_u64())
                })
                .unwrap(),
            Some(1)
        );
        assert!(other
            .read_positioned_range_syntax(&context, 0, other.len(), |_| Ok(()))
            .is_err());
        let error = source
            .read_positioned_range_syntax(&context, 0, source.len(), |_| -> Result<()> {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&first.path)?
                    .write_all(b" ")?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("source changed"));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn escaped_string_chunks_match_serde_and_propagate_callback_failure() {
        for prefix in [0, 1, 7, 65530, 65531, 65532, 65533, 65534, 65535, 65536] {
            let wanted = format!(
                "{}😀\n\t\r\u{8}\u{c}\\\"/é{}",
                "x".repeat(prefix),
                "tail😀\n".repeat(if prefix < 65530 { 1 } else { 20000 })
            );
            let encoded = serde_json::to_string(&wanted).unwrap();
            let mut actual = String::new();
            decode_string(&encoded, &mut |chunk| {
                assert!(chunk.len() <= 65536);
                actual.push_str(chunk);
                Ok(())
            })
            .unwrap();
            assert_eq!(actual, wanted);
            assert!(decode_string(&encoded, &mut |_| anyhow::bail!("sink failure"))
                .unwrap_err()
                .to_string()
                .contains("sink failure"));
        }
    }

    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let dir = std::env::temp_dir().join(format!("cv-json-foundation-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("source.json");
            std::fs::write(&path, bytes).unwrap();
            Self { dir, path }
        }
        fn value(&self) -> Json {
            Json::source(&self.path, 0, std::fs::metadata(&self.path).unwrap().len()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }
    fn canonical(value: &Json) -> Vec<u8> {
        let mut output = Vec::new();
        value.write_canonical(&mut output).unwrap();
        assert_eq!(value.canonical_len().unwrap(), output.len() as u64);
        output
    }

    fn log_fixture(name: &str, bytes: &[u8]) -> (Fixture, PathBuf) {
        let fixture = Fixture::new(b"");
        let path = fixture.dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        (fixture, path)
    }

    #[test]
    fn append_only_log_grown_after_capture_still_reads_its_stable_prefix() {
        let (_fixture, path) = log_fixture("live.jsonl", b"{\"a\":1}\n{\"b\":2}\n");
        let source = JsonSource::capture(&path).unwrap();
        let captured = source.len();
        let file = std::fs::File::open(&path).unwrap();
        let mut writer = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut writer, b"{\"c\":3}\n{\"partial\":").unwrap();
        source.check().unwrap();
        source.check_metadata(file.metadata().unwrap()).unwrap();
        let value = source.validated_range(0, 7).unwrap();
        assert_eq!(canonical(&value), br#"{"a":1}"#);
        assert_eq!(source.len(), captured);
        let range = JsonSource::capture(&path).unwrap();
        range.check().unwrap();
    }

    #[test]
    fn append_only_log_truncated_or_rewritten_after_capture_is_still_a_source_change() {
        for (label, mutate) in [
            (
                "truncate",
                (|p: &Path| std::fs::write(p, b"{\"a\":1}\n").unwrap()) as fn(&Path),
            ),
            ("same-length rewrite", |p| {
                std::fs::write(p, b"{\"z\":9}\n{\"b\":2}\n").unwrap();
                std::fs::File::open(p)
                    .unwrap()
                    .set_modified(SystemTime::UNIX_EPOCH)
                    .unwrap();
            }),
            ("replace with a larger file", |p| {
                let next = p.with_extension("next");
                std::fs::write(&next, b"{\"z\":9}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
                std::fs::rename(&next, p).unwrap();
            }),
        ] {
            let (_fixture, path) = log_fixture("live.jsonl", b"{\"a\":1}\n{\"b\":2}\n");
            let source = JsonSource::capture(&path).unwrap();
            mutate(&path);
            let error = source.check().unwrap_err();
            assert!(error.to_string().contains("JSON source changed"), "{label}: {error}");
        }
    }

    #[test]
    fn whole_document_growth_is_still_a_source_change() {
        let (_fixture, path) = log_fixture("doc.json", b"{\"a\":1}");
        let source = JsonSource::capture(&path).unwrap();
        std::fs::write(&path, b"{\"a\":1} ").unwrap();
        assert!(source.check().unwrap_err().to_string().contains("JSON source changed"));
    }

    #[test]
    fn public_source_guard_factory_retains_absence_and_rejects_stale_dependencies() {
        let fixture = Fixture::new(br#"{"arg":true}"#);
        let sidecar = fixture.dir.join("conditional-wal");
        let mut reads = crate::source::ReadSet::default();
        reads.record(&sidecar);
        let value = Json::source_with_reads(&fixture.path, 0, 12, &reads).unwrap();
        let Json::Source(range) = &value else {
            panic!("checked source")
        };
        let mut consumer = crate::source::ReadSet::default();
        consumer.merge(range.dependencies().unwrap());
        assert!(consumer.fresh(&mut crate::source::Probe::default()));
        std::fs::write(&sidecar, "adopted dependency").unwrap();
        assert!(!consumer.fresh(&mut crate::source::Probe::default()));
        assert!(value.canonical_prefix(1).is_err());
        assert!(Json::source_with_reads(&fixture.path, 0, 12, &reads).is_err());
        reads = crate::source::ReadSet::default();
        reads.record(&sidecar);
        assert!(Json::source_with_reads(&fixture.path, 0, 12, &reads).is_ok());
        assert!(Json::source_with_reads(&fixture.path, 1, 3, &reads).is_err());
    }

    #[test]
    fn canonical_short_escape_strings_match_serde_value() {
        let bodies = [
            r#"plain☺ "#,
            r#"a\n\r\t\b\f\"\\z"#,
            r#"a\/b"#,
            r#"a\u0000\u000A\u0022\u005c\u0061\uD83D\uDE00"#,
            r#"\\u0041\\/"#,
        ];
        for body in bodies {
            let raw = format!(r#"{{"z":-0,"a":["{body}",{{"x":"{body}"}}],"z":2}}"#);
            let fixture = Fixture::new(raw.as_bytes());
            let expected: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(canonical(&fixture.value()), serde_json::to_vec(&expected).unwrap());
        }
    }

    #[test]
    fn typed_document_projection_preserves_derive_duplicate_and_sequence_rules() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Doc {
            #[serde(default)]
            body: Option<Value>,
        }
        let mut cases = vec![
            r#"{"body":null,"body":0}"#.to_owned(),
            r#"{"body":0,"\u0062ody":0}"#.to_owned(),
            r#"{"ignored":1,"ignored":2,"body":0}"#.to_owned(),
            r#"{"ignored\uD800":0}"#.to_owned(),
            r#"{"body":1e400}"#.to_owned(),
            r#"{"ignored":1e400,"body":0}"#.to_owned(),
            "[]".to_owned(),
            "[null]".to_owned(),
            "[0,1]".to_owned(),
        ];
        cases.push(format!(r#"{{"{}\uD800":0}}"#, "x".repeat(100_000)));
        for depth in 125..=129 {
            cases.push(format!(r#"{{"body":{}0{}}}"#, "[".repeat(depth), "]".repeat(depth)));
        }
        for bytes in cases {
            let fixture = Fixture::new(bytes.as_bytes());
            let source = JsonSource::capture(&fixture.path).unwrap();
            let actual = source.read_syntax(|view| {
                let fields = document::Fields::parse_struct(view, &["body"])?.context("not a struct")?;
                if let Some(body) = fields.get("body") {
                    anyhow::ensure!(body.accepts_value_at_depth(1)?, "invalid body");
                }
                Ok(())
            });
            assert_eq!(actual.is_ok(), serde_json::from_str::<Doc>(&bytes).is_ok(), "{bytes}");
            if bytes.contains("ignored\\uD800") || bytes.contains("\\u0062ody") || bytes == "[0,1]" {
                assert!(actual.unwrap_err().downcast_ref::<document::InvalidStruct>().is_some());
            }
        }
    }

    #[test]
    fn document_projection_preserves_escaped_keys_final_values_and_complete_arguments() {
        let body = "雪\"\\".repeat(4000);
        let bytes = format!(
            " \n{{\"ignored{}\":null,\"id\":\"old\",\"\\u0069d\":\"last\",\"body\":{},\"input\":{{\"payload\":{},\"n\":2}}}} \n",
            "x".repeat(20000),
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&body).unwrap()
        );
        let fixture = Fixture::new(bytes.as_bytes());
        let source = JsonSource::capture(&fixture.path).unwrap();
        source
            .read_document(|view| {
                let fields = document::Fields::parse(view, &["id", "id", "body", "input"])?.unwrap();
                assert_eq!(fields.string("id")?.as_deref(), Some("last"));
                let text = fields.get("body").unwrap().text(true)?.unwrap();
                let crate::lazy::Text::Span(span) = text else {
                    panic!("large text must remain a span")
                };
                assert!(span.escaped);
                assert_eq!(
                    crate::lazy::Resolver::new(Some(fixture.path.clone()))
                        .try_resolve(&span)
                        .unwrap(),
                    body
                );
                let argument = fields.get("input").unwrap().argument(true)?;
                assert!(matches!(argument, Json::Source(_)));
                assert_eq!(
                    argument.resolve()?.get("payload").and_then(Value::as_str),
                    Some(body.as_str())
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn archive_whole_grammar_and_independent_value_budgets_match_raw_children() {
        let nested = format!("{}0{}", "[".repeat(126), "]".repeat(126));
        let fixture = Fixture::new(
            format!("[{{\"bad\":1e400}},{{\"bad\":\"\\ud800\"}},{{\"ok\":{nested}}},{{\"ok\":1}}]").as_bytes(),
        );
        let source = JsonSource::capture(&fixture.path).unwrap();
        assert!(source.read_document(|_| Ok(())).is_err());
        let syntax = source
            .read_syntax(|view| {
                view.accepted_array()?
                    .unwrap()
                    .map(|v| v.map(|v| v.is_some()))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(syntax, [false, false, true, true]);
        let accepted = source
            .read_archive(|array| {
                array
                    .unwrap()
                    .map(|value| value.map(|v| v.is_some()))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(accepted, [false, false, true, true]);
        for bytes in [b"[{\"ok\":1},]".as_slice(), b"[{\"ok\":1},\"\xff\"]"] {
            let fixture = Fixture::new(bytes);
            let source = JsonSource::capture(&fixture.path).unwrap();
            let mut published = false;
            let error = source
                .read_archive(|_| {
                    published = true;
                    Ok(())
                })
                .unwrap_err();
            assert!(error.is::<InvalidJson>());
            assert!(!published);
        }
    }

    #[test]
    fn owned_wire_and_mutation_are_value_transparent() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Json>();
        let values = [
            Value::Null,
            json!(true),
            json!(-7),
            json!(1.25),
            json!(["x", null, {"a": 1}]),
            json!({"Source": {"path": "missing", "offset": 0, "len": 100}, "Owned": null}),
        ];
        for value in values {
            let expected = serde_json::to_vec(&value).unwrap();
            let mut wrapped = Json::from(value.clone());
            assert!(matches!(wrapped.resolve().unwrap(), Cow::Borrowed(_)));
            assert_eq!(serde_json::to_vec(&wrapped).unwrap(), expected);
            assert_eq!(canonical(&wrapped), expected);
            let decoded: Json = serde_json::from_slice(&expected).unwrap();
            assert!(matches!(decoded, Json::Owned(_)));
            assert_eq!(*decoded.resolve().unwrap(), value);
            *wrapped.make_owned().unwrap() = json!("mutated");
            assert_eq!(canonical(&wrapped), br#""mutated""#);
        }
    }

    #[test]
    fn source_writer_and_selected_serde_match_independent_value_oracle() {
        let mut cases: Vec<String> = [
            "null",
            "true",
            "false",
            "-0",
            "1.234e+10",
            "1e-400",
            "18446744073709551616",
            "-99999999999999999999999",
            r#""literal UTF8 é\n\r\t\b\f\/\\\"\u0000\u001f\ud83c\udf19""#,
            r#"{"z":0,"a":1,"z":{"b":2,"a":3,"b":4},"a":[true,null,"\u0041"],"":5}"#,
            r#"{"\u0061":1,"b":2,"a":3,"\u0062":4}"#,
            "[[],{},[1,2],\"\",{\"x\": [false, -0, 1e2]}]",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for split in 0..=12 {
            cases.push(format!(
                "{{\"prefix\":\"{}\\ud83c\\udf19é\\n{}\",\"tail\":1}}",
                "x".repeat(65_536 - split),
                "\\\\".repeat(32_769)
            ));
        }
        // Large escaped controls exercise the serializer's worst-case per-window expansion.
        cases.push(format!("\"{}\"", "\\u0000\\t\\n".repeat(20_000)));
        for input in cases {
            let oracle: Value = serde_json::from_str(&input).unwrap();
            let expected = serde_json::to_vec(&oracle).unwrap();
            let fixture = Fixture::new(input.as_bytes());
            let value = fixture.value();
            assert_eq!(canonical(&value), expected);
            assert_eq!(serde_json::to_vec(&value).unwrap(), expected);
            assert_eq!(*value.resolve().unwrap(), oracle);
        }
    }

    #[test]
    fn source_acceptance_matches_value_including_overwritten_invalid_values() {
        let mut cases: Vec<Vec<u8>> = [
            "",
            " ",
            "[] trailing",
            "[1,]",
            "{",
            "00",
            "1e400",
            "-1e400",
            r#""\ud800""#,
            r#""\udc00""#,
            r#"{"a":"\ud800","a":0}"#,
            r#"{"a":1e400,"a":0}"#,
            r#""\ud800\u0041""#,
            r#""\q""#,
            "\"raw\ncontrol\"",
            " \n true \r ",
            r#""\\ud800""#,
        ]
        .into_iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        cases.push(b"{\"unused\":\"\xff\"}".to_vec());
        cases.push(format!("\"{}\\ud800\"", "x".repeat(131_072)).into_bytes());
        cases.push("9".repeat(2000).into_bytes());
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes());
            cases.push(format!("{}0{}", "{\"a\":".repeat(depth), "}".repeat(depth)).into_bytes());
        }
        for input in cases {
            let expected = serde_json::from_slice::<Value>(&input).is_ok();
            let fixture = Fixture::new(&input);
            #[cfg(feature = "mmap")]
            if let Ok(text) = std::str::from_utf8(&input) {
                let source = JsonSource::capture(&fixture.path).unwrap();
                assert_eq!(
                    source.read_borrowed_range_value(0, text, |_| Ok(())).is_ok(),
                    expected,
                    "borrowed {input:?}"
                );
            }
            assert_eq!(
                Json::source(&fixture.path, 0, input.len() as u64).is_ok(),
                expected,
                "{input:?}"
            );
        }
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn borrowed_source_guard_checks_after_callback_failure() {
        let input = "{\"s\":\"body\"}";
        let fixture = Fixture::new(input.as_bytes());
        let source = JsonSource::capture(&fixture.path).unwrap();
        let error = source
            .read_borrowed_range_value(0, input, |_| -> Result<()> {
                std::fs::write(&fixture.path, b"null").unwrap();
                anyhow::bail!("callback failure")
            })
            .unwrap_err();
        assert!(error.to_string().contains("source changed"), "{error}");
    }

    #[test]
    fn scoped_projections_match_owned_and_preserve_duplicate_order() {
        let input = r#"{"z":{"n":1.5,"b":false},"a":[true,null,"\ud83c\udf19"],"z":{"n":2,"b":true},"s":"head\n尾"}"#;
        let fixture = Fixture::new(input.as_bytes());
        let owned = Json::from(serde_json::from_str::<Value>(input).unwrap());
        for value in [owned, fixture.value()] {
            let (keys, strings) = value
                .with_view(|view| {
                    assert!(view.get("missing")?.is_none());
                    let z = view.get("z")?.unwrap();
                    assert_eq!(z.get("n")?.unwrap().as_number()?, Some(Number::from(2)));
                    assert_eq!(z.get("b")?.unwrap().as_bool()?, Some(true));
                    assert!(z.get("n")?.unwrap().get("anything")?.is_none());
                    let array = view.get("a")?.unwrap().array()?.unwrap().collect::<Result<Vec<_>>>()?;
                    assert_eq!(array.len(), 3);
                    assert_eq!(array[0].as_bool()?, Some(true));
                    assert!(array[1].is_null()?);
                    assert!(view.is_object()?);
                    assert!(view.get("a")?.unwrap().is_array()?);
                    assert!(array[2].is_string()?);
                    assert!(!array[1].is_string()?);
                    assert!(!array[1].for_each_str_chunk(|_| panic!("nonstring callback"))?);
                    let mut keys = Vec::new();
                    view.for_each_entry(|key, _| {
                        keys.push(key.to_string());
                        Ok(())
                    })?;
                    let mut strings = String::new();
                    array[2].for_each_str_chunk(|part| {
                        strings.push_str(part);
                        Ok(())
                    })?;
                    view.get("s")?.unwrap().for_each_str_chunk(|part| {
                        strings.push_str(part);
                        Ok(())
                    })?;
                    Ok((keys, strings))
                })
                .unwrap();
            assert_eq!(keys, ["z", "a", "s"]);
            assert_eq!(strings, "🌙head\n尾");
        }
    }

    #[test]
    fn exact_range_bounds_and_read_errors_never_become_null_or_missing() {
        let body = b" {\"value\":\"range\"} \n";
        // Keep non-JSON prefix/suffix outside the selected range.
        let fixture = Fixture::new(&[b"junk".as_slice(), body.as_slice(), b"trailing".as_slice()].concat());
        let mut value = Json::source(&fixture.path, 4, body.len() as u64).unwrap();
        assert_eq!(canonical(&value), br#"{"value":"range"}"#);
        assert!(Json::source(&fixture.path, 0, 4).is_err());
        assert!(Json::source(&fixture.path, u64::MAX, 2).is_err());
        assert!(Json::source(&fixture.path, 0, u64::MAX).is_err());
        assert!(Json::source(&fixture.path, 1000, 1).is_err());
        assert!(Json::source(fixture.dir.join("absent"), 0, 1).is_err());
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.resolve().is_err());
        assert!(value.with_view(|view| Ok(view.get("absent")?.is_none())).is_err());
        assert!(value.canonical_len().is_err());
        assert!(serde_json::to_vec(&value).is_err());
        assert!(value.make_owned().is_err());
        assert!(matches!(value, Json::Source(_)));
    }

    #[test]
    fn source_identity_rejects_append_replacement_and_mutation_during_projection() {
        let fixture = Fixture::new(br#"{"a":1}"#);
        let value = fixture.value();
        std::fs::write(&fixture.path, br#"{"a":1} "#).unwrap();
        assert!(value.canonical_len().is_err());
        let value = fixture.value();
        value
            .with_view(|view| {
                assert_eq!(view.get("a")?.unwrap().as_number()?, Some(Number::from(1)));
                // Preserve length so mapped bytes remain valid; a deliberate mtime makes the check
                // deterministic on filesystems with coarse timestamp granularity.
                std::fs::write(&fixture.path, br#"{"a":2} "#)?;
                std::fs::File::open(&fixture.path)?.set_modified(SystemTime::UNIX_EPOCH)?;
                Ok(())
            })
            .expect_err("post-operation source change must reject its output");
        let value = fixture.value();
        let replacement = fixture.dir.join("replacement");
        std::fs::write(&replacement, br#"{"a":3} "#).unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::rename(replacement, &fixture.path).unwrap();
        #[cfg(unix)]
        assert!(value.canonical_len().is_err()); // Same size/mtime; inode distinguishes replacement.
        drop(value);
        let mut value = fixture.value();
        let expected = value.resolve().unwrap().into_owned();
        assert_eq!(*value.make_owned().unwrap(), expected);
        std::fs::remove_file(&fixture.path).unwrap();
        assert_eq!(*value.resolve().unwrap(), expected); // Explicit ownership survives source loss.
    }

    #[test]
    fn selected_children_share_validated_identity_and_borrowed_owned_views() {
        let input = br#" {"ignored":"large","wrapper":{"n":7,"list":[{"a":"first"},{"a":"last"}]}} "#;
        let fixture = Fixture::new(input);
        let snapshot = JsonSource::capture(&fixture.path).unwrap();
        let index = document::Index::default();
        let raw: &crate::lazy::RawValue = serde_json::from_slice(input).unwrap();
        assert!(document::accepts_value(Raw::new(raw.get(), &index)));
        let value = snapshot.validated_range(1, input.len() as u64 - 2).unwrap();
        let wrapper = value.select("wrapper").unwrap().unwrap();
        let child = wrapper
            .with_view(|view| view.get("list")?.unwrap().array()?.unwrap().nth(1).unwrap()?.to_json())
            .unwrap();
        assert_eq!(canonical(&child), br#"{"a":"last"}"#);
        assert_eq!(canonical(&wrapper.select("n").unwrap().unwrap()), b"7");
        assert!(wrapper.select("missing").unwrap().is_none());
        assert!(matches!(&child, Json::Source(range) if range.path() == fixture.path && range.offset() > 1));
        let public_value = fixture.value();
        assert_eq!(
            canonical(&public_value.select("wrapper").unwrap().unwrap()),
            canonical(&wrapper)
        );
        let owned: Value = serde_json::from_slice(input).unwrap();
        assert!(matches!(JsonView::from(&owned).resolve().unwrap(), Cow::Borrowed(_)));
        assert_eq!(
            canonical(
                &JsonView::from(&owned)
                    .get("wrapper")
                    .unwrap()
                    .unwrap()
                    .to_json()
                    .unwrap()
            ),
            canonical(&wrapper)
        );
        std::fs::File::open(&fixture.path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(child.resolve().is_err());
        assert!(snapshot.validated_range(1, input.len() as u64 - 2).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_restored_mtime_and_symlink_retargeting() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(br#"{"a":1}"#);
        let original_time = std::fs::metadata(&fixture.path).unwrap().modified().unwrap();
        let value = fixture.value();
        std::fs::write(&fixture.path, br#"{"a":2}"#).unwrap();
        std::fs::File::open(&fixture.path)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
        assert!(value.resolve().is_err()); // Unix change time captures the restored mtime edit.
        let other = fixture.dir.join("other");
        std::fs::write(&other, br#"{"a":3}"#).unwrap();
        let link = fixture.dir.join("link");
        symlink(&fixture.path, &link).unwrap();
        let value = Json::source(&link, 0, 7).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();
        assert!(value.resolve().is_err());
    }

    #[test]
    fn direct_source_prefix_reuses_positioned_writer_with_value_semantics() {
        let inputs = [
            serde_json::to_string(&"é🌙\\\n尾".repeat(200_000)).unwrap(),
            format!("[1,2,3,{}]", serde_json::to_string(&"tail".repeat(500_000)).unwrap()),
            r#"{"z":"first","a":[1,true,"é🌙"],"z":"last","tail":null}"#.to_string(),
        ];
        for input in inputs {
            let fixture = Fixture::new(input.as_bytes());
            let source = fixture.value();
            let owned = Json::from(serde_json::from_str::<Value>(&input).unwrap());
            for limit in [0, 1, 2, 5, 17, 65, 4096] {
                #[cfg(feature = "mmap")]
                NATIVE_VIEW_BYTES.with(|bytes| bytes.set(0));
                assert_eq!(
                    source.canonical_prefix(limit).unwrap(),
                    owned.canonical_prefix(limit).unwrap()
                );
                #[cfg(feature = "mmap")]
                assert_eq!(
                    NATIVE_VIEW_BYTES.with(|bytes| bytes.get()),
                    0,
                    "bounded prefix must not request whole native UTF-8 views"
                );
            }
            std::fs::remove_file(&fixture.path).unwrap();
            assert!(source.canonical_prefix(0).is_err());
        }
    }

    #[test]
    fn canonical_prefix_is_bounded_unicode_complete_and_reports_truncation() {
        let input = r#"{"z":"é🌙尾\n","a":[true,1.25],"z":"é🌙尾\n","last":"tail"}"#;
        let fixture = Fixture::new(input.as_bytes());
        let owned: Value = serde_json::from_str(input).unwrap();
        for pretty in [false, true] {
            let expected = if pretty {
                serde_json::to_string_pretty(&owned).unwrap()
            } else {
                serde_json::to_string(&owned).unwrap()
            };
            for value in [Json::from(owned.clone()), fixture.value()] {
                for cap in 0..=expected.len() + 4 {
                    let (prefix, truncated) = value
                        .with_view(|view| {
                            if pretty {
                                view.pretty_prefix(cap)
                            } else {
                                view.canonical_prefix(cap)
                            }
                        })
                        .unwrap();
                    let mut end = cap.min(expected.len());
                    while !expected.is_char_boundary(end) {
                        end -= 1;
                    }
                    assert_eq!(prefix, expected[..end]);
                    assert_eq!(truncated, cap < expected.len());
                    assert!(prefix.capacity() <= cap);
                }
                let (prefix, truncated) = value
                    .with_view(|view| {
                        if pretty {
                            view.pretty_prefix(usize::MAX)
                        } else {
                            view.canonical_prefix(usize::MAX)
                        }
                    })
                    .unwrap();
                assert_eq!(prefix, expected);
                assert!(!truncated);
            }
        }
        let value = fixture.value();
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.canonical_prefix(0).is_err());
    }

    #[test]
    fn prefix_stop_cannot_mask_a_source_change() {
        let fixture = Fixture::new(br#"{"a":1}"#);
        let value = fixture.value();
        let error = value
            .with_view(|view| {
                let result = view.canonical_prefix(0)?;
                std::fs::File::open(&fixture.path)?.set_modified(SystemTime::UNIX_EPOCH)?;
                Ok(result)
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
        let value = fixture.value();
        let error = value
            .with_view::<()>(|_| {
                std::fs::File::open(&fixture.path)?
                    .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
    }

    #[test]
    fn encoded_arguments_keep_backing_children_transport_and_original_identity() {
        let inner = format!(r#" {{"z":"{}","a":1,"z":"last"}} "#, "\\n".repeat(800_000));
        let encoded = serde_json::to_vec(&inner).unwrap();
        let fixture = Fixture::new(&encoded);
        let value = fixture.value().with_view(|view| view.encoded_argument()).unwrap();
        assert!(canonical(&value) == serde_json::to_vec(&serde_json::from_str::<Value>(&inner).unwrap()).unwrap());
        let child = value.select("z").unwrap().unwrap();
        let mut transported = value.clone();
        let mut registry = SourceRegistry::default();
        let descriptor = registry.detach(&mut transported).unwrap();
        drop(value);
        let restored = registry.restore(descriptor).unwrap();
        assert_eq!(canonical(&restored), br#"{"z":"last","a":1}"#);
        assert_eq!(canonical(&child), br#""last""#);
        let error = restored
            .with_view::<()>(|_| {
                std::fs::File::open(&fixture.path)?.set_modified(SystemTime::UNIX_EPOCH)?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(restored.resolve().is_err());
        assert!(child.canonical_len().is_err());
    }

    #[test]
    fn memory_backing_ranges_do_not_read_prefix_suffix_or_recapture_provenance() {
        let fixture = Fixture::new(b"original provenance");
        let source = JsonSource::capture(&fixture.path).unwrap();
        let body = br#"{"p":[0,{"n":42}]}"#;
        let bytes = [b"prefix ".as_slice(), body.as_slice(), b" suffix".as_slice()].concat();
        let backing = Arc::new(crate::harness::store::bytes::ByteSource::read(bytes.as_slice()).unwrap());
        let child = source
            .read_backing_syntax(backing, 7, body.len() as u64, |view| {
                view.get("p")?
                    .unwrap()
                    .array()?
                    .unwrap()
                    .nth(1)
                    .unwrap()?
                    .get("n")?
                    .unwrap()
                    .to_json()
            })
            .unwrap();
        assert_eq!(canonical(&child), b"42");
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(child.resolve().is_err());
    }

    #[test]
    fn same_arena_unescaped_windows_copy_before_append_write() {
        const CHILD: &str = "CV_TEST_SAME_ARENA_APPEND_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "json::tests::same_arena_unescaped_windows_copy_before_append_write",
                ])
                .env(CHILD, "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "same-arena append child failed: {status}");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    // A Darwin vnode deadlock can remain uninterruptible even after kill.
                    // Do not block the test runner waiting for that kernel fault to unwind.
                    let _ = child.try_wait();
                    panic!("same-arena append child {} stalled for 30 seconds", child.id());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let fixture = Fixture::new(b"compressed provenance placeholder");
        let captured = JsonSource::capture(&fixture.path).unwrap();
        let backing = Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let padding = " ".repeat(crate::lazy::INLINE_MAX + 1);
        for argument in [
            "true".to_owned(),
            format!("{padding}true"),
            format!("{padding}{{}}"),
            format!("{padding}[]"),
            padding,
        ] {
            let outer = serde_json::to_vec(&serde_json::json!({"arguments":argument})).unwrap();
            let (offset, len) = backing
                .append(|out| {
                    out.write_all(&outer)?;
                    Ok(())
                })
                .unwrap();
            let value = captured
                .read_backing_value(backing.clone(), offset, len, |view| {
                    view.get("arguments")?.unwrap().encoded_argument()
                })
                .unwrap();
            let expected = crate::harness::wire::openai::parse_arguments(Some(&serde_json::Value::String(argument)));
            assert_eq!(value.resolve().unwrap().as_ref(), &expected);
        }
        // Tiny raw scalars and giant unescaped JSON strings both use borrowed writer fast paths.
        // The destination is the same arena vnode as the mapped input, exercising the Darwin
        // deadlock that producer-owned input buffers and escaped chunks did not reproduce.
        for body in [serde_json::json!(true), serde_json::json!("x".repeat(3 * 64 * 1024))] {
            let wire = serde_json::to_vec(&body).unwrap();
            let (offset, len) = backing
                .append(|out| {
                    out.write_all(&wire)?;
                    Ok(())
                })
                .unwrap();
            let (copy_offset, copy_len) = captured
                .read_backing_value(backing.clone(), offset, len, |view| {
                    backing.append(|mut out| view.write_canonical(&mut out))
                })
                .unwrap();
            let copied = captured
                .read_backing_value(backing.clone(), copy_offset, copy_len, |view| view.to_json())
                .unwrap();
            assert_eq!(canonical(&copied), wire);
        }
    }

    #[test]
    fn nested_encoded_document_uses_same_arena_without_read_append_deadlock() {
        let inner = format!("{{\"payload\":\"{}\"}}", "x".repeat(9000));
        let outer = serde_json::to_vec(&serde_json::json!({"arguments":inner})).unwrap();
        let fixture = Fixture::new(b"compressed provenance placeholder");
        let captured = JsonSource::capture(&fixture.path).unwrap();
        let backing = Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let (offset, len) = backing
            .append(|writer| {
                writer.write_all(&outer)?;
                Ok(())
            })
            .unwrap();
        let value = captured
            .read_backing_syntax(backing.clone(), offset, len, |view| {
                view.get("arguments")?.unwrap().encoded_argument()
            })
            .unwrap();
        let Json::Source(range) = &value else {
            panic!("decoded source")
        };
        assert!(Arc::ptr_eq(range.decoded.as_ref().unwrap(), &backing));
        assert!(range.offset >= len);
        assert!(canonical(&value) == inner.as_bytes());
        assert!(backing.len().unwrap() > len);
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.canonical_len().is_err());
    }

    #[test]
    fn decoded_arguments_share_one_append_arena_and_transport_owner() {
        let inner = [
            format!("{{\"p\":\"{}\"}}", "a".repeat(9000)),
            format!("{{\"p\":\"{}\"}}", "b".repeat(9000)),
        ];
        let fixture = Fixture::new(&serde_json::to_vec(&inner).unwrap());
        let values = fixture
            .value()
            .with_view(|view| {
                view.array()?
                    .unwrap()
                    .map(|view| view?.encoded_argument())
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        let (Json::Source(first), Json::Source(second)) = (&values[0], &values[1]) else {
            panic!("source arguments")
        };
        assert!(Arc::ptr_eq(
            first.decoded.as_ref().unwrap(),
            second.decoded.as_ref().unwrap()
        ));
        assert!(first.offset < second.offset);
        for (value, expected) in values.iter().zip(&inner) {
            assert!(canonical(value) == expected.as_bytes());
        }
        let mut registry = SourceRegistry::default();
        let descriptors = values
            .into_iter()
            .map(|mut value| registry.detach(&mut value).unwrap())
            .collect::<Vec<_>>();
        for (descriptor, expected) in descriptors.into_iter().zip(&inner) {
            assert!(canonical(&registry.restore(descriptor).unwrap()) == expected.as_bytes());
        }
    }

    #[test]
    fn encoded_arguments_preserve_invalid_scalar_blank_and_strict_value_semantics() {
        for inner in [
            format!("{{\"x\":\"{}\"}}", "x".repeat(9000)),
            format!("{}0{}{}", "[".repeat(130), "]".repeat(130), " ".repeat(9000)),
            format!("{{\"x\":\"\\ud800\",\"p\":\"{}\"}}", "x".repeat(9000)),
            format!("{{\"x\":1e999,\"p\":\"{}\"}}", "x".repeat(9000)),
            format!("42{}", " ".repeat(9000)),
            " ".repeat(9000),
            format!("{{\"x\":\"{}", "x".repeat(9000)),
        ] {
            let fixture = Fixture::new(&serde_json::to_vec(&inner).unwrap());
            let value = fixture.value().with_view(|view| view.encoded_argument()).unwrap();
            assert_eq!(
                canonical(&value),
                serde_json::to_vec(&crate::harness::wire::openai::structured_arguments(&inner)).unwrap()
            );
        }
    }

    struct FailingWriter {
        remaining: usize,
        written: usize,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("injected writer failure"));
            }
            let n = bytes.len().min(self.remaining);
            self.remaining -= n;
            self.written += n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn writer_and_string_reducer_errors_are_propagated() {
        let input = format!("{{\"a\":\"{}\"}}", "\\n".repeat(100_000));
        let fixture = Fixture::new(input.as_bytes());
        for value in [
            Json::from(serde_json::from_str::<Value>(&input).unwrap()),
            fixture.value(),
        ] {
            let mut writer = FailingWriter {
                remaining: 100,
                written: 0,
            };
            let error = value.write_canonical(&mut writer).unwrap_err();
            assert!(error.to_string().contains("injected writer failure"));
            assert_eq!(writer.written, 100);
            let mut calls = 0;
            let error = value
                .with_view(|view| {
                    view.get("a")?.unwrap().for_each_str_chunk(|_| {
                        calls += 1;
                        anyhow::bail!("injected reducer failure")
                    })
                })
                .unwrap_err();
            assert_eq!(calls, 1);
            assert!(error.to_string().contains("injected reducer failure"));
        }
    }
}
