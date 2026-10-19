//! Primary identity and auxiliary source dependencies, with append-safe comparisons.
//!
//! A consumer captures a [`Baseline`] before it reads: the primary file's [`FileId`] and a
//! [`Stamp`] of every auxiliary path, present or absent, never a lossy aggregate. It publishes
//! the baseline with its output in one transaction, as one flat record whose bytes carry the
//! digest of that output; loading verifies the record, and the consumer verifies the digest
//! against its row. Each consumer owns its baseline: refreshing counts cannot make an older
//! search index fresh, and a missing or legacy record rereads once.
//!
//! These are metadata guards, not content verification. Append-safe comparisons deliberately
//! exclude the primary identity: immutable-prefix contracts and sampled resume marks govern reuse
//! during growth. The before-pass inventory covers adapter-declared paths; consumers additionally
//! capture paths before dynamic reads. Complete read sets (even empty) stay distinct from
//! unrecorded ones.

use crate::ir::{Harness, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Cursor Agent CLI/ACP metadata participates in both catalog discovery and consumer freshness.
/// Include the absent path: creating it can supply a previously missing cwd or title.
pub(crate) fn cursor_cli_metadata(r: &SessionRef) -> Option<PathBuf> {
    (r.harness == Harness::Cursor && r.id.starts_with("cli:"))
        .then(|| r.path.parent().map(|parent| parent.join("meta.json")))
        .flatten()
}

/// Native SQLite identities retain sidecars even when a harness permits an arbitrary filename.
/// Unreadable arbitrary OpenCode sources are unknown, so consumers cannot certify freshness.
pub(crate) fn is_sqlite_source(r: &SessionRef) -> Option<bool> {
    if r.path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3" | "vscdb"))
    {
        return Some(true);
    }
    if r.harness != Harness::OpenCode {
        return Some(false);
    }
    use std::io::Read;
    let mut file = std::fs::File::open(&r.path).ok()?;
    let mut header = [0; 16];
    match file.read_exact(&mut header) {
        Ok(()) => Some(&header == b"SQLite format 3\0"),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Some(false),
        Err(_) => None,
    }
}

pub(crate) fn sqlite_sidecars(path: &Path) -> [PathBuf; 2] {
    ["-wal", "-journal"].map(|suffix| {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        PathBuf::from(sidecar)
    })
}

// Preserve the path spelling: canonicalize cannot represent missing files and would hide a
// symlink's later retargeting. Relative reads belong to the cwd of the pass that consulted them.
fn absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

/// One file as `stat` saw it, the one identity every guard compares. It has exactly two
/// comparisons: `==` (strict: the file is unchanged) and [`FileId::admits`] (append: the same
/// file, grown or not, with its captured prefix intact). Device, inode and ctime are 0 off Unix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub len: u64,
    pub mtime_ns: u64,
    pub dev: u64,
    pub ino: u64,
    pub ctime_ns: i64,
}

impl FileId {
    /// The file at `path` now; `None` when it is absent or unreadable.
    pub fn at(path: &Path) -> Option<Self> {
        Self::of(&std::fs::metadata(path).ok()?)
    }

    pub fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let modified = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        #[cfg(unix)]
        let (dev, ino, ctime_ns) = (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
        );
        #[cfg(not(unix))]
        let (dev, ino, ctime_ns) = (0, 0, 0);
        Some(Self {
            len: metadata.len(),
            mtime_ns: modified.as_nanos() as u64,
            dev,
            ino,
            ctime_ns,
        })
    }

    /// The five words, little-endian: the bytes a stored identity is.
    pub fn to_bytes(self) -> [u8; 40] {
        let mut out = [0; 40];
        for (i, word) in [self.len, self.mtime_ns, self.dev, self.ino, self.ctime_ns as u64]
            .into_iter()
            .enumerate()
        {
            out[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// [`Self::to_bytes`] reversed; `None` for any other length.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 40] = bytes.try_into().ok()?;
        let word = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Some(Self {
            len: word(0),
            mtime_ns: word(1),
            dev: word(2),
            ino: word(3),
            ctime_ns: word(4) as i64,
        })
    }

    /// Append admission for an append-only source captured as `self` with sample `prefix`:
    /// `later` is this file unchanged, or the same file (device and inode) at least as long whose
    /// bytes under the captured length still sample as they did ([`Prefix::holds`]). No timing:
    /// a writer mid-`write` (new timestamps, old length) is admitted by content, and a same-length
    /// rewrite of the sampled bytes is rejected. An edit outside the samples is not detected
    /// (the limit [`crate::resume`] documents).
    pub fn admits(&self, prefix: &Prefix, later: &FileId, path: &Path) -> bool {
        self == later || (later.dev, later.ino) == (self.dev, self.ino) && later.len >= self.len && prefix.holds(path)
    }
}

/// How many bytes a [`Prefix`] samples at the head of the file and again just before its end.
pub(crate) const SAMPLE_SPAN: u64 = 4096;

/// How many bytes one read of [`prefix_sha256`] takes at a time: a bounded buffer over a prefix
/// that can be gigabytes.
const PREFIX_READ_SPAN: u64 = 64 * 1024;

/// The bytes `0..len` of one file (device, inode), sampled: FNV-1a over the first
/// [`SAMPLE_SPAN`] bytes, the [`SAMPLE_SPAN`] bytes before `len` (overlapping on a short file)
/// and `len` itself. A sample, not a digest of the whole prefix: a caller whose claim covers
/// every byte of the prefix compares `prefix_sha256` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Prefix {
    pub len: u64,
    pub hash: u64,
    pub dev: u64,
    pub ino: u64,
}

impl Prefix {
    /// The sample of `file`'s first `len` bytes; `None` when it is shorter or unreadable. Reads
    /// positioned, so the handle's own position never moves.
    pub fn of(file: &std::fs::File, len: u64) -> Option<Self> {
        use crate::harness::store::jsonl::ranges::read_exact_at;
        let id = FileId::of(&file.metadata().ok()?)?;
        if id.len < len {
            return None;
        }
        let mut head = vec![0u8; len.min(SAMPLE_SPAN) as usize];
        read_exact_at(file, &mut head, 0).ok()?;
        let from = len.saturating_sub(SAMPLE_SPAN);
        let mut tail = vec![0u8; (len - from) as usize];
        read_exact_at(file, &mut tail, from).ok()?;
        let hash = fnv1a(fnv1a(fnv1a(0xcbf2_9ce4_8422_2325, &head), &tail), &len.to_le_bytes());
        Some(Self {
            len,
            hash,
            dev: id.dev,
            ino: id.ino,
        })
    }

    /// The file `path` names now is this one, at least `len` long, sampling as it did.
    pub fn holds(&self, path: &Path) -> bool {
        std::fs::File::open(path).is_ok_and(|file| Self::of(&file, self.len) == Some(*self))
    }
}

/// SHA-256 over the whole `0..len` bytes of `file` as they are now — the comparison [`Prefix`]
/// only samples, so a same-length edit of the prefix's middle is visible here and invisible there.
/// Positioned reads in bounded windows, so the handle's own position never moves and no prefix is
/// ever held in memory; `None` when the file is shorter than `len` or unreadable.
pub(crate) fn prefix_sha256(file: &std::fs::File, len: u64) -> Option<String> {
    use crate::harness::store::jsonl::ranges::read_exact_at;
    let mut hasher = crate::digest::Sha256::new();
    let mut buf = vec![0u8; len.min(PREFIX_READ_SPAN) as usize];
    let mut at = 0u64;
    while at < len {
        let n = (len - at).min(PREFIX_READ_SPAN) as usize;
        read_exact_at(file, &mut buf[..n], at).ok()?;
        hasher.update(&buf[..n]);
        at += n as u64;
    }
    Some(hasher.finish_hex())
}

/// One recorded path: its identity, or `None` when absent. A pass's append-only primary also
/// keeps its [`Prefix`] in memory, which only that pass's growth admission reads; equality,
/// hashing and the serialized record are path and identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    state: Option<FileId>,
    #[serde(skip)]
    prefix: Option<Prefix>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.state == other.state
    }
}
impl Eq for Entry {}
impl std::hash::Hash for Entry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.state.hash(state);
    }
}

/// Exact serialized auxiliary records, not a content digest.
#[derive(Serialize, Deserialize)]
struct Records {
    version: u32,
    entries: Vec<Entry>,
    /// None is a before-pass inventory, not a complete consumer read set.
    #[serde(default)]
    reads: Option<Vec<Entry>>,
}

/// The one format version of stamps and stored records; older ones reread once.
const VERSION: u32 = 4;

/// A canonical record string. Loaded baselines remain strings instead of allocating each path
/// three times for counts, events and search; equality compares the actual records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp(String);

impl Stamp {
    fn serialize(records: &Records) -> Option<Self> {
        let mut text = serde_json::to_string(records).ok()?;
        // These operation/checkpoint strings live across publication checks. Serializer growth
        // capacity is unused afterward and otherwise doubles retained inventory storage.
        text.shrink_to_fit();
        Some(Self(text))
    }

    fn records(&self) -> Option<Records> {
        let records: Records = serde_json::from_str(&self.0).ok()?;
        (records.version == VERSION).then_some(records)
    }

    /// The same inventory, and every recorded read still as recorded. A complete empty read set
    /// against a freshly generated stamp compares as text, without decoding its owned paths.
    fn matches(&self, current: &Stamp, probe: &mut Probe) -> bool {
        if let Some(base) = self.0.strip_suffix(",\"reads\":[]}") {
            if [",\"reads\":null}", ",\"reads\":[]}"]
                .iter()
                .any(|suffix| current.0.strip_suffix(suffix) == Some(base))
            {
                return true;
            }
        }
        let Some(old) = self.records() else { return false };
        let Some(current) = current.records() else { return false };
        old.entries == current.entries
            && old.reads.as_ref().is_some_and(|reads| {
                reads
                    .iter()
                    .all(|entry| probe.entry(&entry.path).as_ref() == Some(entry))
            })
    }
}

mod scope;
pub(crate) use scope::{inventory, inventory_scoped};

/// Lifetime of a multi-session source operation. SQLite cold-store snapshots are reused while
/// this guard or any reader is alive and removed when the last owner drops. Without an outer
/// guard, standalone session reads remain safe but may repeat copying on non-cloning filesystems.
#[must_use]
pub struct ReadScope {
    _inventory: scope::Context,
    #[cfg(feature = "sqlite")]
    _sqlite: crate::harness::store::sqlite::ReadScope,
}

impl ReadScope {
    /// Run a worker under this operation's inventory owner, restoring its prior thread context
    /// even on panic. The guard remains owned by the caller for the complete operation.
    pub fn run<T>(&self, operation: impl FnOnce() -> T) -> T {
        self._inventory.run(operation)
    }
}

pub fn read_scope() -> ReadScope {
    ReadScope {
        _inventory: scope::Context::operation(),
        #[cfg(feature = "sqlite")]
        _sqlite: crate::harness::store::sqlite::read_scope(),
    }
}

/// Capture an active inventory owner for existing parallel helpers, without creating one for
/// unrelated parallel consumers. Each worker restores its previous thread-local capability.
pub(crate) fn inventory_context() -> Option<scope::Context> {
    scope::Context::current()
}

/// The metadata captured immediately before a consumer reads dynamically resolved paths.
/// Captures absence too. Repeated paths retain the first stamp; publication revalidates it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReadSet {
    entries: BTreeMap<PathBuf, Entry>,
    known: bool,
}

impl Default for ReadSet {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            known: true,
        }
    }
}

impl ReadSet {
    /// A legacy checkpoint did not capture the dependencies of its reused prefix. It may be
    /// replayed, but cannot establish cache freshness until a complete pass replaces it.
    pub fn unrecorded() -> Self {
        Self {
            entries: BTreeMap::new(),
            known: false,
        }
    }

    pub fn record(&mut self, path: &Path) {
        self.record_as(path, false);
    }

    /// [`Self::record`]; for an `appendable` primary its first record also keeps its [`Prefix`],
    /// so [`Self::fresh_or_grown`] can admit its growth by content.
    pub(crate) fn record_as(&mut self, path: &Path, appendable: bool) {
        let Some(path) = absolute(path) else {
            self.known = false;
            return;
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(path) {
            match Probe::default().entry(slot.key()) {
                Some(mut entry) => {
                    if let Some(state) = entry.state.filter(|_| appendable) {
                        entry.prefix = std::fs::File::open(&entry.path)
                            .ok()
                            .and_then(|file| Prefix::of(&file, state.len));
                    }
                    slot.insert(entry);
                }
                None => self.known = false,
            }
        }
    }

    /// The `(mtime_ns, len)` recorded for an existing `path`, the key persisted caches index by.
    pub(crate) fn signature(&self, path: &Path) -> Option<(u64, u64)> {
        let state = self.entries.get(&absolute(path)?)?.state.as_ref()?;
        Some((state.mtime_ns, state.len))
    }

    /// Paths whose original stamps this read set retains. Dependency notifications can replay
    /// these paths while guarded projections/publication continue to carry the original set.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.entries.keys().map(PathBuf::as_path)
    }

    /// Combine consumer reads, retaining the first metadata observed for each path.
    pub fn merge(&mut self, other: &Self) {
        self.known &= other.known;
        for (path, entry) in &other.entries {
            self.entries.entry(path.clone()).or_insert_with(|| entry.clone());
        }
    }

    /// Captured reads may include auxiliary sources beyond a caller's primary baseline.
    pub(crate) fn includes(&self, baseline: &Self) -> bool {
        self.known
            && baseline.known
            && baseline
                .entries
                .iter()
                .all(|(path, entry)| self.entries.get(path) == Some(entry))
    }

    /// Revalidate exactly the paths captured by this consumer, including absence and identity.
    pub fn fresh(&self, probe: &mut Probe) -> bool {
        self.known
            && self
                .entries
                .values()
                .all(|entry| probe.entry(&entry.path).as_ref() == Some(entry))
    }

    /// [`Self::fresh`], except that a primary recorded as appendable may also
    /// have grown ([`FileId::admits`]): a pass that read inside its captured length still
    /// describes that prefix.
    pub(crate) fn fresh_or_grown(&self, probe: &mut Probe) -> bool {
        self.known
            && self.entries.values().all(|entry| {
                let now = probe.entry(&entry.path);
                now.as_ref() == Some(entry)
                    || entry
                        .state
                        .zip(entry.prefix)
                        .zip(now.and_then(|now| now.state))
                        .is_some_and(|((then, prefix), now)| then.admits(&prefix, &now, &entry.path))
            })
    }
}

/// Record the original and auxiliary sources retained by one message before a consumer demands it.
pub(crate) fn session_reads(session: &crate::Session) -> ReadSet {
    let mut reads = ReadSet::default();
    let primary = session.source_path.as_deref().unwrap_or_else(|| Path::new(""));
    if session.source_path.is_some() {
        reads.record(primary);
    }
    for message in &session.messages {
        record_message_reads(message, primary, &mut reads);
    }
    reads
}

/// Record the original and auxiliary sources retained by one message before a consumer demands it.
pub(crate) fn record_message_reads(message: &crate::Message, primary: &std::path::Path, reads: &mut ReadSet) {
    if let Some(record) = &message.complete_record {
        if let crate::Json::Source(range) = &record.value {
            if range.path() != primary {
                reads.record(range.path());
            }
            if let Some(guard) = range.dependencies() {
                reads.merge(guard);
            }
        }
    }
    for block in &message.content {
        if let crate::Block::ToolUse {
            input: crate::Json::Source(range),
            ..
        } = block
        {
            if range.path() != primary {
                reads.record(range.path());
            }
            if let Some(guard) = range.dependencies() {
                reads.merge(guard);
            }
        }
        let text = match block {
            crate::Block::Text { text } | crate::Block::Thinking { text, .. } => Some(text),
            crate::Block::ToolResult { content, .. } => Some(content),
            _ => None,
        };
        match text {
            Some(crate::Text::Span(span)) => {
                if let Some(path) = span.source.as_ref().filter(|path| path.as_path() != primary) {
                    reads.record(path);
                }
            }
            Some(crate::Text::Source(source)) => {
                if source.path() != primary {
                    reads.record(source.path());
                }
                if let Some(guard) = source.dependencies() {
                    reads.merge(guard);
                }
            }
            _ => {}
        }
    }
}

/// What a consumer captured before reading one session: the primary's identity and the stamp of
/// its auxiliary paths. Compared one way: [`Baseline::fresh`] (strict) or
/// [`Baseline::dependencies_fresh`] (auxiliary only, across primary growth).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub primary: Option<FileId>,
    pub stamp: Option<Stamp>,
}

impl Baseline {
    /// The captured primary's length; 0 when it was absent.
    pub fn primary_len(&self) -> u64 {
        self.primary.map_or(0, |id| id.len)
    }

    /// Retain both declared inventory and conditional reads with their original identities.
    pub(crate) fn all_reads(&self) -> Option<ReadSet> {
        let records = self.stamp.as_ref()?.records()?;
        let mut reads = ReadSet::default();
        for entry in records.entries.into_iter().chain(records.reads.into_iter().flatten()) {
            reads.entries.entry(entry.path.clone()).or_insert(entry);
        }
        Some(reads)
    }

    /// Recorded consumer reads, for a resumed consumer to retain reads made by its old prefix.
    pub fn reads(&self) -> Option<ReadSet> {
        let entries = self.stamp.as_ref()?.records()?.reads?;
        Some(ReadSet {
            entries: entries.into_iter().map(|entry| (entry.path.clone(), entry)).collect(),
            known: true,
        })
    }

    /// Auxiliary inventories and all recorded reads still match, independently of primary growth.
    pub fn dependencies_fresh(&self, current: &Self, probe: &mut Probe) -> bool {
        self.stamp
            .as_ref()
            .zip(current.stamp.as_ref())
            .is_some_and(|(old, now)| old.matches(now, probe))
    }

    /// Strict: the primary was captured and is unchanged, and the dependencies are fresh.
    pub fn fresh(&self, current: &Self, probe: &mut Probe) -> bool {
        self.primary.is_some() && self.primary == current.primary && self.dependencies_fresh(current, probe)
    }

    /// Attach the read set captured by this consumer's complete pass, never another consumer's.
    pub fn with_reads(&self, reads: &ReadSet) -> Self {
        let stamp = (|| {
            if !reads.known {
                return None;
            }
            let mut records = self.stamp.as_ref()?.records()?;
            records.reads = Some(reads.entries.values().cloned().collect());
            Stamp::serialize(&records)
        })();
        Self {
            primary: self.primary,
            stamp,
        }
    }
}

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The digest a record carries of the output row it was published with.
fn output_digest(output: &[u8]) -> u64 {
    fnv1a(0xcbf2_9ce4_8422_2325, output)
}

/// A stored baseline, the bytes `source_dependencies` holds: `VERSION u32 | output digest u64 |
/// primary present u8 | len, mtime_ns, dev, ino u64, ctime_ns i64 | stamp`, little-endian.
/// Decoding verifies the version and the length; [`Published::of`] verifies the output.
pub struct Published {
    baseline: Baseline,
    output: u64,
}

const HEAD: usize = 4 + 8 + 1 + 5 * 8;

impl Published {
    #[cfg(feature = "sqlite")]
    fn encode(baseline: &Baseline, output: &[u8]) -> Option<Vec<u8>> {
        let stamp = baseline.stamp.as_ref()?;
        let mut bytes = Vec::with_capacity(HEAD + stamp.0.len());
        bytes.extend(VERSION.to_le_bytes());
        bytes.extend(output_digest(output).to_le_bytes());
        bytes.push(u8::from(baseline.primary.is_some()));
        bytes.extend(baseline.primary.unwrap_or_default().to_bytes());
        bytes.extend(stamp.0.as_bytes());
        Some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEAD || bytes[..4] != VERSION.to_le_bytes() || bytes[12] > 1 {
            return None;
        }
        Some(Self {
            baseline: Baseline {
                primary: (bytes[12] == 1).then(|| FileId::from_bytes(&bytes[13..HEAD])).flatten(),
                stamp: Some(Stamp(String::from_utf8(bytes[HEAD..].to_vec()).ok()?)),
            },
            output: u64::from_le_bytes(bytes[4..12].try_into().unwrap()),
        })
    }

    /// The stored baseline, when `output` is the output it was published with.
    pub fn of(&self, output: &[u8]) -> Option<&Baseline> {
        (self.output == output_digest(output)).then_some(&self.baseline)
    }
}

/// What an adapter declares a session reads besides its primary
/// ([`crate::harness::Adapter::dependencies`]): `files` are recorded present or absent;
/// `directories` are enumerated one level and every entry recorded.
#[derive(Default)]
pub struct Dependencies {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
}

/// File and immediate-directory probes shared within one operation. It never descends recursively.
#[derive(Default)]
pub struct Probe {
    // The map key owns the path once; outer None is unavailable, inner None is absent.
    entries: HashMap<PathBuf, Option<Option<FileId>>>,
    directories: HashMap<PathBuf, Option<std::sync::Arc<Vec<PathBuf>>>>,
    // Adapters resolved once per operation: detection stats roots, so not once per session.
    adapters: HashMap<Harness, Option<Box<dyn crate::harness::Adapter>>>,
}

impl Probe {
    fn entry(&mut self, path: &Path) -> Option<Entry> {
        let path = absolute(path)?;
        let state = (*self
            .entries
            .entry(path.clone())
            .or_insert_with(|| match std::fs::metadata(&path) {
                Ok(md) => Some(Some(FileId::of(&md)?)),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Some(None)
                }
                Err(_) => None,
            }))?;
        Some(Entry {
            path,
            state,
            prefix: None,
        })
    }

    /// Whether `path` exists, recording its identity for this operation; `None` when unreadable.
    pub fn exists(&mut self, path: &Path) -> Option<bool> {
        Some(self.entry(path)?.state.is_some())
    }

    /// The entries of directory `path`, read once per operation; an absent directory is empty.
    pub fn list(&mut self, path: &Path) -> Option<std::sync::Arc<Vec<PathBuf>>> {
        let path = absolute(path)?;
        self.directories
            .entry(path.clone())
            .or_insert_with(|| match std::fs::read_dir(&path) {
                Ok(rd) => rd
                    .map(|e| e.ok().map(|e| e.path()))
                    .collect::<Option<Vec<_>>>()
                    .map(std::sync::Arc::new),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(std::sync::Arc::new(Vec::new())),
                Err(_) => None,
            })
            .clone()
    }

    /// [`primary_path`] with this operation's adapter registry.
    pub fn primary_path(&mut self, r: &SessionRef) -> PathBuf {
        self.adapters
            .entry(r.harness)
            .or_insert_with(|| crate::harness::for_harness(r.harness))
            .as_ref()
            .map_or_else(|| r.path.clone(), |adapter| adapter.resume_source(r))
    }

    fn dependencies_from(&mut self, r: &SessionRef, primary: &Path) -> Option<Stamp> {
        Stamp::serialize(&Records {
            version: VERSION,
            entries: self.dependency_entries(r, primary)?,
            reads: None,
        })
    }

    // Publication already has typed captured records; collect fresh entries without a second
    // serialized stamp and parser allocation. The same inventory serves serialized checkpoints.
    fn dependency_entries(&mut self, r: &SessionRef, primary: &Path) -> Option<Vec<Entry>> {
        let mut deps = Dependencies::default();
        if is_sqlite_source(r)? {
            deps.files.extend(sqlite_sidecars(&r.path));
        }
        deps.files.extend(cursor_cli_metadata(r));
        // The adapter is taken out of the registry while it probes through `self`.
        let adapter = self
            .adapters
            .remove(&r.harness)
            .unwrap_or_else(|| crate::harness::for_harness(r.harness));
        let declared = adapter
            .as_ref()
            .map_or(Some(()), |adapter| adapter.dependencies(r, self, &mut deps));
        self.adapters.insert(r.harness, adapter);
        declared?;
        let Dependencies { files, directories } = deps;
        let mut entries = BTreeMap::new();
        for dir in directories {
            entries.insert(dir.clone(), self.entry(&dir)?.state);
            for file in self.list(&dir)?.iter() {
                if primary != r.path && absolute(file)? == absolute(primary)? {
                    continue;
                }
                let state = self.entry(file)?.state;
                entries.insert(file.clone(), state);
            }
        }
        for file in files {
            // A directory adapter's append file belongs to the primary signature and sampled
            // checkpoint guard. Keeping it in the auxiliary set would reject every append.
            // Preserve existing shared-database inventories when the logical path is the primary.
            if primary != r.path && absolute(&file)? == absolute(primary)? {
                continue;
            }
            let state = self.entry(&file)?.state;
            entries.insert(file, state);
        }
        entries
            .into_iter()
            .map(|(path, state)| {
                let path = if path.is_absolute() { path } else { absolute(&path)? };
                Some(Entry {
                    path,
                    state,
                    prefix: None,
                })
            })
            .collect()
    }

    pub fn snapshot(&mut self, r: &SessionRef) -> Baseline {
        let primary = self.primary_path(r);
        self.snapshot_from(r, &primary)
    }

    /// Capture the concrete adapter's append source while retaining its logical session key
    /// and all other auxiliary inputs. Path selection must not perform unrecorded source reads.
    pub fn snapshot_from(&mut self, r: &SessionRef, primary: &Path) -> Baseline {
        Baseline {
            primary: self.entry(primary).and_then(|entry| entry.state),
            stamp: self.dependencies_from(r, primary),
        }
    }
}

/// The built-in adapter's actual append file, which may differ from a directory session key.
pub fn primary_path(r: &SessionRef) -> PathBuf {
    crate::harness::for_harness(r.harness).map_or_else(|| r.path.clone(), |adapter| adapter.resume_source(r))
}

/// A captured source still matches at publication time. Errors never establish freshness.
pub fn unchanged(r: &SessionRef, baseline: &Baseline) -> bool {
    unchanged_from(r, &primary_path(r), baseline)
}

/// Publication guard against the concrete adapter's source path.
pub fn unchanged_from(r: &SessionRef, primary: &Path, baseline: &Baseline) -> bool {
    let mut probe = Probe::default();
    if baseline.primary.is_none() || probe.entry(primary).and_then(|entry| entry.state) != baseline.primary {
        return false;
    }
    let Some(stamp) = baseline.stamp.as_ref() else {
        return false;
    };
    let Some(now) = probe.dependency_entries(r, primary) else {
        return false;
    };
    // Directory enumeration is complete. Only metadata probes are needed for dynamic reads;
    // release path lists before parsing the captured inventory's owned paths.
    probe.directories.clear();
    let Some(Records { entries, reads, .. }) = stamp.records() else {
        return false;
    };
    let same = entries == now;
    drop(entries);
    drop(now);
    same && reads.as_ref().is_none_or(|reads| {
        reads
            .iter()
            .all(|entry| probe.entry(&entry.path).as_ref() == Some(entry))
    })
}

pub type Update = (SessionRef, Baseline);

type Key = (String, String, String);
fn key(r: &SessionRef) -> Key {
    (
        r.harness.as_str().into(),
        r.id.clone(),
        r.path.to_string_lossy().into_owned(),
    )
}

/// A consumer's published baselines, loaded once. Older records and missing read sets reread
/// once; even empty complete read sets have a row, so an old consumer cannot silently certify
/// unresolved paths.
#[derive(Default)]
pub struct Table {
    rows: HashMap<Key, Published>,
}

impl Table {
    /// This consumer's baseline for `r`, for [`Baseline::dependencies_fresh`] across growth.
    pub fn get(&self, r: &SessionRef) -> Option<&Baseline> {
        Some(&self.rows.get(&key(r))?.baseline)
    }

    /// The baseline published with exactly `output`, the consumer's current output row.
    pub fn published(&self, r: &SessionRef, output: &[u8]) -> Option<&Baseline> {
        self.rows.get(&key(r))?.of(output)
    }

    /// The consumer's output `output` for `r` is [`Baseline::fresh`] against `now`.
    pub fn fresh(&self, r: &SessionRef, now: &Baseline, output: &[u8], probe: &mut Probe) -> bool {
        self.published(r, output).is_some_and(|old| old.fresh(now, probe))
    }

    #[cfg(feature = "sqlite")]
    pub fn load(consumer: &str) -> Self {
        open().map_or_else(Self::default, |conn| Self::load_in(&conn, consumer))
    }

    /// Load alongside SQL consumer rows in the caller's read transaction.
    #[cfg(feature = "sqlite")]
    pub(crate) fn load_in(conn: &rusqlite::Connection, consumer: &str) -> Self {
        let load = || -> Option<_> {
            let mut stmt = conn
                .prepare("SELECT harness,id,path,record FROM source_dependencies WHERE consumer=?1")
                .ok()?;
            let rows = stmt
                .query_map([consumer], |row| {
                    Ok(((row.get(0)?, row.get(1)?, row.get(2)?), row.get::<_, Vec<u8>>(3)?))
                })
                .ok()?;
            Some(
                rows.flatten()
                    .filter_map(|(key, record)| Some((key, Published::decode(&record)?)))
                    .collect(),
            )
        };
        Self {
            rows: load().unwrap_or_default(),
        }
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn load(_consumer: &str) -> Self {
        Self { rows: HashMap::new() }
    }
}

#[cfg(feature = "sqlite")]
fn open() -> Option<rusqlite::Connection> {
    let conn = crate::catalog::open_db()?;
    ensure_table(&conn).ok()?;
    Some(conn)
}

/// The table, in the one-record shape. A table of the older columns is dropped: every consumer
/// rereads once.
#[cfg(feature = "sqlite")]
fn ensure_table(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let legacy: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('source_dependencies') WHERE name='stamp')",
        [],
        |row| row.get(0),
    )?;
    if legacy {
        conn.execute_batch("DROP TABLE source_dependencies;")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_dependencies(
        consumer TEXT NOT NULL,harness TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,
        record BLOB NOT NULL,PRIMARY KEY(consumer,harness,id,path));",
    )
}

/// One stored baseline, when it was published with `output`. The consumer row and this record
/// must be read from one SQL snapshot by the caller.
#[cfg(feature = "sqlite")]
pub(crate) fn published_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    output: &[u8],
) -> Option<Baseline> {
    let (h, id, path) = key(r);
    let record: Vec<u8> = conn
        .query_row(
            "SELECT record FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
            rusqlite::params![consumer, h, id, path],
            |row| row.get(0),
        )
        .ok()?;
    Published::decode(&record)?.of(output).cloned()
}

/// [`Table::fresh`] for one row, without loading every session's records during writes.
#[cfg(feature = "sqlite")]
pub(crate) fn fresh_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    now: &Baseline,
    output: &[u8],
) -> bool {
    published_in(conn, consumer, r, output).is_some_and(|old| old.fresh(now, &mut Probe::default()))
}

/// Publish in the same transaction as SQL consumer output `output`. A failure, returned with
/// its reason, must roll back both. Only a complete pass (its read set recorded) over a source
/// still unchanged publishes.
#[cfg(feature = "sqlite")]
pub(crate) fn publish_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    baseline: &Baseline,
    output: &[u8],
) -> anyhow::Result<()> {
    ensure_table(conn)?;
    let (h, id, path) = key(r);
    conn.execute(
        "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        rusqlite::params![consumer, h, id, path],
    )?;
    let complete = baseline
        .stamp
        .as_ref()
        .is_some_and(|stamp| stamp.records().is_some_and(|records| records.reads.is_some()));
    if complete && unchanged(r, baseline) {
        let record = Published::encode(baseline, output).ok_or_else(|| anyhow::anyhow!("baseline without a stamp"))?;
        conn.execute(
            "INSERT INTO source_dependencies(consumer,harness,id,path,record) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![consumer, h, id, path, record],
        )?;
    }
    Ok(())
}

/// Forget every baseline of `consumer` on the caller's connection: its output is gone.
#[cfg(feature = "sqlite")]
pub(crate) fn forget_in(conn: &rusqlite::Connection, consumer: &str) -> Option<()> {
    ensure_table(conn).ok()?;
    conn.execute("DELETE FROM source_dependencies WHERE consumer=?1", [consumer])
        .ok()?;
    Some(())
}

/// Move a consumer's baselines to a new name. Rows the new name already has are newer and kept.
#[cfg(feature = "sqlite")]
pub fn rename_consumer(from: &str, to: &str) -> bool {
    let rename = || -> Option<()> {
        let mut conn = open()?;
        let tx = conn.transaction().ok()?;
        tx.execute(
            "INSERT OR IGNORE INTO source_dependencies(consumer,harness,id,path,record)
             SELECT ?2,harness,id,path,record FROM source_dependencies WHERE consumer=?1",
            rusqlite::params![from, to],
        )
        .ok()?;
        tx.execute("DELETE FROM source_dependencies WHERE consumer=?1", [from])
            .ok()?;
        tx.commit().ok()
    };
    rename().is_some()
}

#[cfg(not(feature = "sqlite"))]
pub fn rename_consumer(_from: &str, _to: &str) -> bool {
    true
}

/// Invalidate old baselines before changing consumer output. Failure must prevent that output
/// write: otherwise an older stamp could certify newer output after dependencies revert.
#[cfg(feature = "sqlite")]
pub fn invalidate(consumer: &str, refs: &[SessionRef]) -> bool {
    if refs.is_empty() {
        return true;
    }
    let clear = || -> Option<()> {
        let mut conn = open()?;
        let tx = conn.transaction().ok()?;
        for r in refs {
            let (h, id, path) = key(r);
            tx.execute(
                "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
                rusqlite::params![consumer, h, id, path],
            )
            .ok()?;
        }
        tx.commit().ok()
    };
    clear().is_some()
}

#[cfg(not(feature = "sqlite"))]
pub fn invalidate(_consumer: &str, _refs: &[SessionRef]) -> bool {
    true
}

/// Publish after a consumer whose output lives outside the catalog commits it (its output is
/// the empty row). Revalidation makes concurrent edits stale; old baselines must already have
/// been invalidated with [`invalidate`]. SQL consumers publish atomically through `publish_in`.
/// Each row publishes on its own: the rows returned failed, with the reason, and only their
/// sessions are read again next pass; the rest commit together.
#[cfg(feature = "sqlite")]
pub fn update(consumer: &str, updates: &[Update]) -> Vec<(SessionRef, String)> {
    if updates.is_empty() {
        return Vec::new();
    }
    let mut failed = Vec::new();
    let mut publish = || -> anyhow::Result<()> {
        let mut conn = open().ok_or_else(|| anyhow::anyhow!("the catalog cannot be opened"))?;
        let mut tx = conn.transaction()?;
        for (r, baseline) in updates {
            let row = tx.savepoint().map_err(anyhow::Error::from).and_then(|row| {
                publish_in(&row, consumer, r, baseline, b"")?;
                Ok(row.commit()?)
            });
            if let Err(e) = row {
                failed.push((r.clone(), format!("{e:#}")));
            }
        }
        Ok(tx.commit()?)
    };
    match publish() {
        Ok(()) => failed,
        Err(e) => updates.iter().map(|(r, _)| (r.clone(), format!("{e:#}"))).collect(),
    }
}

#[cfg(not(feature = "sqlite"))]
pub fn update(_consumer: &str, _updates: &[Update]) -> Vec<(SessionRef, String)> {
    Vec::new()
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    #[test]
    fn arbitrary_opencode_database_names_retain_wal_and_journal_guards() {
        let w = crate::test_world("source-opencode-arbitrary-db");
        let path = w.base.join("custom-store");
        std::fs::write(&path, b"SQLite format 3\0remaining database bytes").unwrap();
        let r = SessionRef::new("session", Harness::OpenCode, &path);
        assert_eq!(is_sqlite_source(&r), Some(true));
        let before = Probe::default().snapshot(&r);
        assert!(unchanged(&r, &before));
        for sidecar in sqlite_sidecars(&path) {
            std::fs::write(&sidecar, "changed native state").unwrap();
            assert!(!unchanged(&r, &before));
            std::fs::remove_file(sidecar).unwrap();
            assert!(unchanged(&r, &before));
        }
        let missing = SessionRef::new("missing", Harness::OpenCode, w.base.join("missing"));
        assert_eq!(is_sqlite_source(&missing), None);
        assert!(Probe::default().snapshot(&missing).stamp.is_none());
        let legacy = w.base.join("ses_old.json");
        std::fs::write(&legacy, "{}").unwrap();
        assert_eq!(
            is_sqlite_source(&SessionRef::new("old", Harness::OpenCode, legacy)),
            Some(false)
        );
    }

    #[test]
    fn grok_dependency_kind_projects_only_exact_subagents_and_keeps_acceptance() {
        let world = crate::test_world("grok-dependency-kind");
        let group = world.base.join("sessions");
        let session = group.join("child");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("chat_history.jsonl"), "").unwrap();
        let reference = crate::SessionRef::new("child", crate::Harness::Grok, session.clone());
        let large = "ignored payload".repeat(1024);
        let cases = [
            serde_json::json!({"session_kind":"subagent", "ignored":large}).to_string(),
            "{\"session_kind\":\"subagent\",\"session_kind\":false}".into(),
            "{\"session_kind\":\"subagentx\"}".into(),
            "null".into(),
            "{\"session_kind\":\"subagent\",\"ignored\":\"\\ud800\"}".into(),
        ];
        for text in cases {
            std::fs::write(session.join("summary.json"), &text).unwrap();
            let old = serde_json::from_str::<serde_json::Value>(&text).ok();
            let stamp = super::Probe::default().snapshot(&reference).stamp;
            assert_eq!(stamp.is_some(), old.is_some());
            if let Some(stamp) = stamp {
                let expected = old.unwrap().get("session_kind").and_then(serde_json::Value::as_str) == Some("subagent");
                assert_eq!(
                    stamp.records().unwrap().entries.iter().any(|entry| entry.path == group),
                    expected
                );
            }
        }
    }

    #[test]
    fn impossible_child_absence_invalidates_when_parent_becomes_directory() {
        let world = crate::test_world("not-directory-absence");
        let parent = world.base.join("parent");
        let child = parent.join("child");
        std::fs::write(&parent, "a file").unwrap();
        let mut reads = ReadSet::default();
        reads.record(&child);
        assert!(reads.fresh(&mut Probe::default()));
        std::fs::remove_file(&parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&child, "now present").unwrap();
        assert!(!reads.fresh(&mut Probe::default()));
    }

    fn tuple(baseline: &Baseline) -> Option<(u64, u64)> {
        baseline.primary.map(|id| (id.len, id.mtime_ns))
    }

    /// A `Prefix` samples; the whole-prefix digest is what a claim about every copied byte needs.
    /// The sample's blindness here is the premise, asserted rather than assumed.
    #[test]
    fn whole_prefix_digest_sees_the_middle_a_sample_cannot_and_ignores_growth() {
        let w = crate::test_world("source-prefix-digest");
        let path = w.base.join("session.jsonl");
        let body = |marker: &str| format!("{}{}{}", "x".repeat(6000), marker, "y".repeat(6000));
        std::fs::write(&path, body("original")).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let at = body("original").find("original").unwrap();
        assert!(
            at as u64 >= SAMPLE_SPAN && at as u64 + 8 <= len - SAMPLE_SPAN,
            "the marker must sit outside both {SAMPLE_SPAN}-byte samples"
        );
        let captured = prefix_sha256(&file, len).unwrap();
        assert_eq!(prefix_sha256(&file, len).unwrap(), captured, "an unchanged prefix");
        assert!(
            prefix_sha256(&file, len + 1).is_none(),
            "a file shorter than the prefix"
        );
        let prefix = Prefix::of(&file, len).unwrap();
        std::fs::write(&path, body("modified")).unwrap();
        assert!(
            prefix.holds(&path),
            "the sample is blind to a same-length middle rewrite"
        );
        let rewritten = prefix_sha256(&file, len).unwrap();
        assert_ne!(rewritten, captured, "the whole-prefix digest is not");
        std::fs::write(&path, format!("{}tail\n", body("modified"))).unwrap();
        assert_eq!(
            prefix_sha256(&file, len).unwrap(),
            rewritten,
            "growth past the captured length leaves the captured prefix's digest alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn primary_identity_rejects_restored_tuple_rewrites_and_replacements() {
        let w = crate::test_world("primary-identity");
        // Include the canonical delimiter in the filename to exercise JSON escaping.
        let path = w.base.join("plain,\"entries\":.jsonl");
        std::fs::write(&path, "old body").unwrap();
        let r = SessionRef::new("s", Harness::Claude, &path);
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        for consumer in ["counts", "events", "fts", "semantic"] {
            update(consumer, &[(r.clone(), before.clone())]);
            assert!(Table::load(consumer)
                .get(&r)
                .is_some_and(|old| old.fresh(&before, &mut Probe::default())));
        }
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "new body").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let now = Probe::default().snapshot(&r);
        assert_eq!(tuple(&before), tuple(&now), "length and mtime restored");
        assert!(!before.fresh(&now, &mut Probe::default()));
        assert!(!unchanged(&r, &before));
        assert!(
            before.dependencies_fresh(&now, &mut Probe::default()),
            "auxiliary prefix authority excludes primary identity"
        );
        for consumer in ["counts", "events", "fts", "semantic"] {
            let table = Table::load(consumer);
            assert!(!table.get(&r).is_some_and(|old| old.fresh(&now, &mut Probe::default())));
            assert!(table.get(&r).is_some_and(
                |old| old.primary == before.primary && old.dependencies_fresh(&now, &mut Probe::default())
            ));
        }
        // A publication racing the restored-tuple write cannot establish freshness.
        update("counts", &[(r.clone(), before.clone())]);
        assert!(!Table::load("counts")
            .get(&r)
            .is_some_and(|old| old.fresh(&now, &mut Probe::default())));
        let now = now.with_reads(&ReadSet::default());
        update("counts", &[(r.clone(), now.clone())]);
        assert!(Table::load("counts")
            .get(&r)
            .is_some_and(|old| old.fresh(&now, &mut Probe::default())));
        assert!(!Table::load("events")
            .get(&r)
            .is_some_and(|old| old.fresh(&now, &mut Probe::default())));
        // Replacement with equal bytes and restored timestamps is a different primary too.
        let replacement = w.base.join("replacement");
        std::fs::write(&replacement, "new body").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let replaced = Probe::default().snapshot(&r);
        assert_eq!(tuple(&now), tuple(&replaced));
        assert!(!now.fresh(&replaced, &mut Probe::default()));
        assert!(!unchanged(&r, &now));
        assert!(!Table::load("counts")
            .get(&r)
            .is_some_and(|old| old.fresh(&replaced, &mut Probe::default())));
    }

    #[test]
    fn stored_records_verify_version_shape_and_output_before_any_inventory_decodes() {
        let w = crate::test_world("record-verify");
        let path = w.base.join("primary.jsonl");
        std::fs::write(&path, "body").unwrap();
        let r = SessionRef::new("s", Harness::Claude, &path);
        let now = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let stamp = now.stamp.as_ref().unwrap();
        assert!(stamp.matches(stamp, &mut Probe::default()));
        let mut legacy = serde_json::to_value(stamp.records().unwrap()).unwrap();
        legacy["version"] = 2.into();
        let legacy = Stamp(serde_json::to_string(&legacy).unwrap());
        assert!(!legacy.matches(stamp, &mut Probe::default()));
        assert!(!Stamp("{}".into()).matches(stamp, &mut Probe::default()));
        let bytes = Published::encode(&now, b"row").unwrap();
        let stored = Published::decode(&bytes).unwrap();
        assert!(stored.of(b"other").is_none());
        assert!(stored.of(b"row").unwrap().fresh(&now, &mut Probe::default()));
        assert_eq!(stored.of(b"row").unwrap().primary, now.primary);
        assert!(Published::decode(&bytes[..HEAD - 1]).is_none());
        let mut other_version = bytes.clone();
        other_version[0] ^= 1;
        assert!(Published::decode(&other_version).is_none());
        let mut absent = bytes.clone();
        absent[12] = 0;
        assert!(!Published::decode(&absent)
            .unwrap()
            .of(b"row")
            .unwrap()
            .fresh(&now, &mut Probe::default()));
        absent[12] = 2;
        assert!(Published::decode(&absent).is_none());
        let mut text = bytes.clone();
        text.truncate(HEAD);
        text.extend(b"{\"version\":1}");
        assert!(!Published::decode(&text)
            .unwrap()
            .of(b"row")
            .unwrap()
            .fresh(&now, &mut Probe::default()));
        assert!(Published::encode(&Probe::default().snapshot(&r), b"").is_some());
        assert!(Published::encode(
            &Baseline {
                primary: now.primary,
                stamp: None
            },
            b""
        )
        .is_none());
    }

    #[test]
    fn sqlite_journal_creation_rewrite_and_removal_invalidate_inventory() {
        let world = crate::test_world("source-sqlite-journal");
        let db = world.base.join("sessions.db");
        std::fs::write(&db, "fixture").unwrap();
        let journal = world.base.join("sessions.db-journal");
        let r = SessionRef::new("s", Harness::Hermes, &db);
        let absent = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let records = absent.stamp.as_ref().unwrap().records().unwrap();
        assert!(records
            .entries
            .iter()
            .any(|entry| entry.path == journal && entry.state.is_none()));
        let mut legacy_records = records;
        legacy_records.entries.retain(|entry| entry.path != journal);
        let legacy = Baseline {
            stamp: Stamp::serialize(&legacy_records),
            ..absent.clone()
        };
        assert!(
            !unchanged(&r, &legacy),
            "old inventories reread once without a version change"
        );
        assert!(unchanged(&r, &absent));
        std::fs::write(&journal, "before").unwrap();
        assert!(!unchanged(&r, &absent));
        let present = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(present.primary, absent.primary);
        assert!(unchanged(&r, &present));
        std::fs::write(&journal, "after!").unwrap();
        assert!(!unchanged(&r, &present), "same-size journal rewrite");
        let rewritten = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::remove_file(&journal).unwrap();
        assert!(!unchanged(&r, &rewritten));
    }

    #[test]
    fn typed_publication_matches_serialized_inventory_and_preserves_wire() {
        let world = crate::test_world("source-typed-publication");
        let storage = world.base.join("storage");
        for dir in ["session/project", "message/sid", "part/mid"] {
            std::fs::create_dir_all(storage.join(dir)).unwrap();
        }
        let path = storage.join("session/project/sid.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(storage.join("message/sid/header.json"), r#"{"id":"mid"}"#).unwrap();
        let part = storage.join("part/mid/part.json");
        std::fs::write(&part, "before").unwrap();
        let r = SessionRef::new("sid", Harness::OpenCode, path);
        let absent = world.base.join("conditional");
        let mut reads = ReadSet::default();
        reads.record(&absent);
        let before = Probe::default().snapshot(&r).with_reads(&reads);
        let stamp = before.stamp.as_ref().unwrap();
        assert_eq!(stamp.0, serde_json::to_string(&stamp.records().unwrap()).unwrap());
        assert_eq!(stamp.0.capacity(), stamp.0.len());
        let reference = |captured: &Baseline| {
            let mut probe = Probe::default();
            let now = probe.snapshot(&r);
            let old = captured.stamp.as_ref().unwrap().records().unwrap();
            let fresh = now.stamp.as_ref().unwrap().records().unwrap();
            now.primary == captured.primary
                && old.entries == fresh.entries
                && old.reads.as_ref().is_none_or(|reads| {
                    reads
                        .iter()
                        .all(|entry| probe.entry(&entry.path).as_ref() == Some(entry))
                })
        };
        assert!(reference(&before));
        assert!(unchanged(&r, &before));
        std::fs::write(&absent, "created").unwrap();
        assert_eq!(unchanged(&r, &before), reference(&before));
        assert!(!unchanged(&r, &before));
        std::fs::remove_file(&absent).unwrap();
        std::fs::write(&part, "after!").unwrap();
        assert_eq!(unchanged(&r, &before), reference(&before));
        assert!(!unchanged(&r, &before));
        let updated = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(storage.join("part/mid/new.json"), "new").unwrap();
        assert_eq!(unchanged(&r, &updated), reference(&updated));
        assert!(!unchanged(&r, &updated));
        let populated = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::remove_file(storage.join("part/mid/new.json")).unwrap();
        std::fs::remove_file(&part).unwrap();
        std::fs::remove_dir(storage.join("part/mid")).unwrap();
        assert_eq!(unchanged(&r, &populated), reference(&populated));
        assert!(
            !unchanged(&r, &populated),
            "directory removal changes captured membership"
        );
    }

    #[test]
    fn cursor_cli_metadata_creation_and_rewrite_invalidate_consumers() {
        let world = crate::test_world("cursor-cli-source-metadata");
        let path = world.base.join("store.db");
        std::fs::write(&path, "database").unwrap();
        let r = crate::SessionRef::new("cli:session", crate::Harness::Cursor, &path);
        let before = super::Probe::default()
            .snapshot(&r)
            .with_reads(&super::ReadSet::default());
        let metadata = world.base.join("meta.json");
        assert!(super::unchanged(&r, &before));
        std::fs::write(&metadata, r#"{"title":"first"}"#).unwrap();
        assert!(!super::unchanged(&r, &before));
        let present = super::Probe::default()
            .snapshot(&r)
            .with_reads(&super::ReadSet::default());
        std::fs::write(&metadata, r#"{"title":"rewritten title"}"#).unwrap();
        assert!(!super::unchanged(&r, &present));
        let ide = crate::SessionRef::new("composer", crate::Harness::Cursor, &path);
        assert!(super::cursor_cli_metadata(&ide).is_none());
    }

    #[test]
    fn selected_directory_primary_grows_without_blessing_metadata_changes() {
        let w = crate::test_world("source-directory-primary");
        let session = w.base.join("session");
        let agent = session.join("agents/agent-child");
        std::fs::create_dir_all(&agent).unwrap();
        let wire = agent.join("wire.jsonl");
        let state = session.join("state.json");
        std::fs::write(&wire, "prefix\n").unwrap();
        std::fs::write(&state, "{}").unwrap();
        let r = crate::SessionRef::new("session/agent-child", crate::Harness::KimiCode, &agent);
        let before = super::Probe::default()
            .snapshot_from(&r, &wire)
            .with_reads(&super::ReadSet::default());
        assert_eq!(before.primary, super::FileId::at(&wire));
        std::fs::write(&wire, "prefix\nappend\n").unwrap();
        let grown = super::Probe::default().snapshot_from(&r, &wire);
        assert_ne!(before.primary, grown.primary);
        assert!(before.dependencies_fresh(&grown, &mut super::Probe::default()));
        assert!(!before.fresh(&grown, &mut super::Probe::default()));
        assert!(!super::unchanged_from(&r, &wire, &before));
        std::fs::write(&state, "{\"title\":\"edited\"}").unwrap();
        let changed = super::Probe::default().snapshot_from(&r, &wire);
        assert!(!before.dependencies_fresh(&changed, &mut super::Probe::default()));
        // Missing controls are stamped too: later creation must invalidate the baseline.
        std::fs::remove_file(&state).unwrap();
        let absent = super::Probe::default()
            .snapshot_from(&r, &wire)
            .with_reads(&super::ReadSet::default());
        std::fs::write(&state, "{}").unwrap();
        assert!(!super::unchanged_from(&r, &wire, &absent));
    }

    #[test]
    fn serialized_consumer_reads_survive_growth_and_reject_dependency_changes() {
        let w = crate::test_world("source-read-state");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let r = crate::SessionRef::new("test", crate::Harness::Codex, &path);
        let extra = w.base.join("external.txt");
        let mut reads = super::ReadSet::default();
        reads.record(&extra);
        let captured = super::Probe::default().snapshot(&r).with_reads(&reads);
        let restored: super::Baseline = serde_json::from_slice(&serde_json::to_vec(&captured).unwrap()).unwrap();
        assert!(restored.fresh(&super::Probe::default().snapshot(&r), &mut super::Probe::default()));
        std::fs::write(&path, "primary grows").unwrap();
        let now = super::Probe::default().snapshot(&r);
        assert!(!restored.fresh(&now, &mut super::Probe::default()));
        assert!(restored.dependencies_fresh(&now, &mut super::Probe::default()));
        let mut suffix = super::ReadSet::default();
        suffix.merge(&restored.reads().unwrap());
        assert!(now.with_reads(&suffix).fresh(&now, &mut super::Probe::default()));
        std::fs::write(&extra, "created").unwrap();
        assert!(!restored.dependencies_fresh(&super::Probe::default().snapshot(&r), &mut super::Probe::default()));
        suffix.merge(&super::ReadSet::unrecorded());
        assert!(now.with_reads(&suffix).stamp.is_none());
    }
    use super::*;

    #[test]
    fn relative_reads_remain_bound_to_the_capture_directory() {
        // cwd belongs to the process, so exercise the change in a child instead of racing the
        // workspace test runner's other adapters and file operations.
        const CHILD_ROOT: &str = "CV_SOURCE_RELATIVE_READS_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let r = SessionRef {
                harness: Harness::Codex,
                id: "relative".into(),
                path: root.join("primary.jsonl"),
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: 0,
            };
            let base = Probe::default().snapshot(&r);
            let mut reads = ReadSet::default();
            reads.record(Path::new("external.txt"));
            assert!(reads.entries.keys().all(|path| path.is_absolute()));
            let captured = base.with_reads(&reads);
            update("relative", &[(r.clone(), captured.clone())]);
            std::env::set_current_dir(root.join("other")).unwrap();
            assert!(unchanged(&r, &captured));
            assert!(Table::load("relative")
                .get(&r)
                .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
            // A different same-named file in the new cwd is irrelevant to the original read.
            std::fs::write("external.txt", "unrelated").unwrap();
            assert!(Table::load("relative")
                .get(&r)
                .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
            std::fs::write(root.join("external.txt"), "after!").unwrap();
            assert!(!unchanged(&r, &captured));
            assert!(!Table::load("relative")
                .get(&r)
                .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
            return;
        }
        let w = crate::test_world("relative-source-reads");
        std::fs::write(w.base.join("primary.jsonl"), "primary").unwrap();
        std::fs::write(w.base.join("external.txt"), "before").unwrap();
        std::fs::create_dir(w.base.join("other")).unwrap();
        let status = w
            .command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "source::tests::relative_reads_remain_bound_to_the_capture_directory",
                "--nocapture",
            ])
            .env(CHILD_ROOT, &w.base)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn dynamic_reads_capture_absence_and_reject_changes_during_a_pass() {
        let w = crate::test_world("dynamic-reads");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let r = SessionRef {
            harness: Harness::Codex,
            id: "test".into(),
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let base = Probe::default().snapshot(&r);
        let extra = w.base.join("external.txt");
        let mut reads = ReadSet::default();
        reads.record(&extra);
        let before = base.with_reads(&reads);
        update("dynamic", &[(r.clone(), before.clone())]);
        assert!(Table::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        std::fs::write(&extra, "before").unwrap();
        reads.record(&extra); // Keep the first capture, even after the dependent read changed.
        assert!(!unchanged(&r, &base.with_reads(&reads)));
        assert!(!Table::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        update("dynamic", &[(r.clone(), before)]);
        assert!(!Table::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        let mut reads = ReadSet::default();
        reads.record(&extra);
        update("dynamic", &[(r.clone(), base.with_reads(&reads))]);
        assert!(Table::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        std::fs::write(&extra, "after!").unwrap();
        assert!(!Table::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        assert_eq!(Probe::default().snapshot(&r).primary, base.primary);
        assert!(!Table::load("other")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
    }

    #[test]
    fn unrecorded_and_version_one_baselines_reread_once() {
        let w = crate::test_world("read-set-upgrade");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let r = SessionRef {
            harness: Harness::Codex,
            id: "test".into(),
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let base = Probe::default().snapshot(&r);
        update("upgrade", &[(r.clone(), base.clone())]);
        assert!(!Table::load("upgrade")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        let complete = base.with_reads(&ReadSet::default());
        update("upgrade", &[(r.clone(), complete.clone())]);
        assert!(Table::load("upgrade")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        let mut table = Table::load("upgrade");
        for old in table.rows.values_mut() {
            let stamp = old.baseline.stamp.as_mut().unwrap();
            stamp.0 = stamp.0.replacen("\"version\":4", "\"version\":1", 1);
        }
        assert!(!table.get(&r).is_some_and(|old| old.fresh(&base, &mut Probe::default())));
        assert!(Table::load("upgrade")
            .get(&r)
            .is_some_and(|old| old.fresh(&base, &mut Probe::default())));
    }

    #[test]
    fn dependency_baselines_are_per_consumer_and_revalidated_at_publication() {
        let w = crate::test_world("source-baselines");
        let path = w.base.join("sessions.db");
        std::fs::write(&path, "database").unwrap();
        let r = SessionRef {
            harness: Harness::Hermes,
            id: "s1".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let mut probe = Probe::default();
        let before = probe.snapshot(&r).with_reads(&ReadSet::default());
        let other = SessionRef {
            id: "s2".into(),
            ..r.clone()
        };
        assert_eq!(
            probe.snapshot(&other).with_reads(&ReadSet::default()).stamp,
            before.stamp
        );
        assert_eq!(
            probe.entries.len(),
            3,
            "one main DB and absent WAL/journal probes shared by both sessions"
        );
        assert!(!Table::load("counts")
            .get(&r)
            .is_some_and(|old| old.fresh(&before, &mut Probe::default())));
        update("counts", &[(r.clone(), before.clone())]);
        assert!(Table::load("counts")
            .get(&r)
            .is_some_and(|old| old.fresh(&before, &mut Probe::default())));
        assert!(!Table::load("events")
            .get(&r)
            .is_some_and(|old| old.fresh(&before, &mut Probe::default())));

        std::fs::write(w.base.join("sessions.db-wal"), "new WAL").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(after.primary, before.primary);
        assert_ne!(after.stamp, before.stamp);
        assert!(!unchanged(&r, &before));
        update("counts", &[(r.clone(), before)]);
        assert!(
            !Table::load("counts")
                .get(&r)
                .is_some_and(|old| old.fresh(&after, &mut Probe::default())),
            "a racing change cannot publish freshness"
        );
        update("counts", &[(r.clone(), after.clone())]);
        assert!(Table::load("counts")
            .get(&r)
            .is_some_and(|old| old.fresh(&after, &mut Probe::default())));
        assert!(!Table::load("events")
            .get(&r)
            .is_some_and(|old| old.fresh(&after, &mut Probe::default())));
    }
    #[test]
    fn scoped_children_track_wire_state_and_opencode_parts() {
        let w = crate::test_world("source-children");
        let session = w.base.join("session_test");
        std::fs::create_dir_all(session.join("agents/main")).unwrap();
        std::fs::write(session.join("state.json"), "before").unwrap();
        let wire = session.join("agents/main/wire.jsonl");
        std::fs::write(&wire, "line1\n").unwrap();
        let r = SessionRef {
            harness: Harness::KimiCode,
            id: "test".into(),
            path: session.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(&wire, "line1\nline2\n").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        // The finalized-step adapter now uses wire.jsonl as its concrete append primary.
        assert_ne!(before.primary, after.primary);
        assert!(before.dependencies_fresh(&after, &mut Probe::default()));
        std::fs::write(session.join("state.json"), "after!").unwrap();
        assert_ne!(
            after.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp
        );

        let storage = w.base.join("storage");
        for dir in ["session/project", "message/sid", "part/mid"] {
            std::fs::create_dir_all(storage.join(dir)).unwrap();
        }
        let path = storage.join("session/project/sid.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(storage.join("message/sid/noncanonical.json"), r#"{"id":"mid"}"#).unwrap();
        let part = storage.join("part/mid/pid.json");
        std::fs::write(&part, "before").unwrap();
        let r = SessionRef {
            harness: Harness::OpenCode,
            id: "sid".into(),
            path,
            ..r
        };
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(&part, "after!").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(before.primary, after.primary);
        assert_ne!(before.stamp, after.stamp, "part-only same-size edit");
        std::fs::write(storage.join("part/mid/new.json"), "new").unwrap();
        assert_ne!(
            after.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp,
            "part membership"
        );
    }

    #[test]
    #[allow(deprecated)]
    fn counts_publish_captured_primary_and_legacy_record_stays_stale() {
        let w = crate::test_world("source-counts-publish");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "before").unwrap();
        let r = SessionRef {
            harness: Harness::Codex,
            id: "test".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        crate::catalog::open_db().unwrap();
        let source = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        crate::catalog::note_affordances_with_source(&r, &source, || {
            // Even an edit restoring the original tuple must not bless stale consumer output.
            std::fs::write(&path, "after!").unwrap();
            #[cfg(unix)]
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            #[cfg(not(unix))]
            let _ = modified;
            crate::affordance::Counts::new()
        });
        assert!(crate::catalog::AffordanceTable::load().stale(&r));
    }
    #[test]
    fn failed_stamp_publication_cannot_reuse_an_older_baseline() {
        let w = crate::test_world("source-publication-failure");
        let path = w.base.join("sessions.db");
        std::fs::write(&path, "database").unwrap();
        let r = SessionRef {
            harness: Harness::Hermes,
            id: "s".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let conn = crate::catalog::open_db().unwrap();
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        crate::catalog::note_affordances_with_source(&r, &before, crate::affordance::Counts::new);
        assert!(!crate::catalog::AffordanceTable::load().stale(&r));
        let wal = w.base.join("sessions.db-wal");
        std::fs::write(&wal, "new").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        conn.execute_batch(
            "CREATE TRIGGER reject_stamp BEFORE INSERT ON source_dependencies
            WHEN NEW.consumer='counts' BEGIN SELECT RAISE(FAIL,'stamp rejected'); END;",
        )
        .unwrap();
        crate::catalog::note_affordances_with_source(&r, &after, || {
            [(crate::affordance::Kind::Plan, 1)].into_iter().collect()
        });
        std::fs::remove_file(&wal).unwrap();
        assert_eq!(
            before.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp
        );
        assert!(
            crate::catalog::AffordanceTable::load().counts(&r).unwrap().is_empty(),
            "failed stamp insert rolls back new counts too"
        );
        conn.execute_batch("DROP TRIGGER reject_stamp;").unwrap();
        crate::catalog::note_affordances_with_source(&r, &before, crate::affordance::Counts::new);
        conn.execute_batch(
            "CREATE TRIGGER reject_invalidation BEFORE DELETE ON source_dependencies
            WHEN OLD.consumer='counts' BEGIN SELECT RAISE(FAIL,'delete rejected'); END;",
        )
        .unwrap();
        std::fs::write(&wal, "new").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let mut called = false;
        crate::catalog::note_affordances_with_source(&r, &after, || {
            called = true;
            crate::affordance::Counts::new()
        });
        assert!(called);
        std::fs::remove_file(&wal).unwrap();
        assert!(
            crate::catalog::AffordanceTable::load().counts(&r).unwrap().is_empty(),
            "failed invalidation rolls back consumer output mutation"
        );
    }
}
