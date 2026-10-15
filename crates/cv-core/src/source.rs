//! Primary identity and auxiliary source dependencies, with append-safe comparisons.
//!
//! Stamps retain canonical individual path records, including absence, rather than a lossy
//! max-mtime/total-size aggregate. Each consumer owns its baseline: refreshing counts cannot
//! make an older search index fresh. Missing baselines cause an incremental reread once.
//!
//! These are metadata freshness guards, not content verification. Version 3 captures primary
//! identity before reads. On Unix, inode/ctime reject timestamp-restored rewrites/replacements;
//! other platforms currently retain the length/mtime metadata guarantee.
//! Append-safe dependency comparisons deliberately exclude that primary identity: immutable-prefix
//! contracts and sampled resume marks still govern reuse during growth. The before-pass inventory
//! covers adapter-local files; consumers additionally capture paths before dynamic reads. Complete
//! read sets (even empty) remain distinct from unrecorded ones. Older baselines reread once through
//! ordinary incremental refresh; no index rebuild.

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

fn sqlite_sidecars(path: &Path) -> [PathBuf; 2] {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    state: Option<FileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct FileState {
    len: u64,
    modified_ns: u64,
    identity: Option<(u64, u64, i64, i64)>,
}

/// Exact serialized primary and auxiliary metadata records, not a content digest.
#[derive(Serialize, Deserialize)]
struct Records {
    version: u32,
    primary: Entry,
    entries: Vec<Entry>,
    /// None is a before-pass inventory, not a complete consumer read set.
    #[serde(default)]
    reads: Option<Vec<Entry>>,
}

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
        (records.version == 3).then_some(records)
    }

    /// Canonical serialization puts the small primary record before the potentially large
    /// inventory. The delimiter cannot occur inside an encoded path: its quotes would be escaped.
    /// Compare it without decoding every auxiliary path. Older/noncanonical records miss.
    fn canonical_parts(&self) -> Option<(&str, &str)> {
        self.0.strip_prefix("{\"version\":3,\"primary\":{")?;
        self.0.split_once(",\"entries\":")
    }

    fn same_primary(&self, current: &Stamp) -> bool {
        self.canonical_parts()
            .zip(current.canonical_parts())
            .is_some_and(|((old, _), (now, _))| old == now)
    }

    fn matches(&self, current: &Stamp, probe: &mut Probe) -> bool {
        // Auxiliary comparison is independent of primary growth. Canonical complete empty
        // read sets compare the remaining inventory without decoding its owned paths.
        if let Some(((old_primary, old), (now_primary, now))) = self.canonical_parts().zip(current.canonical_parts()) {
            if old.strip_suffix("\"reads\":[]}").is_some_and(|base| {
                now.strip_suffix("\"reads\":null}") == Some(base) || now.strip_suffix("\"reads\":[]}") == Some(base)
            }) {
                // Equal primary prefixes are already validated by the freshly generated stamp.
                // A differing prefix must still be a well-formed typed record before resume.
                return old_primary == now_primary
                    || old_primary
                        .strip_prefix("{\"version\":3,\"primary\":")
                        .is_some_and(|entry| serde_json::from_str::<Entry>(entry).is_ok());
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
        let Some(path) = absolute(path) else {
            self.known = false;
            return;
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(path) {
            match Probe::default().entry(slot.key()) {
                Some(entry) => {
                    slot.insert(entry);
                }
                None => self.known = false,
            }
        }
    }

    /// The `(mtime_ns, len)` recorded for an existing `path`, the key persisted caches index by.
    pub(crate) fn signature(&self, path: &Path) -> Option<(u64, u64)> {
        let state = self.entries.get(&absolute(path)?)?.state.as_ref()?;
        Some((state.modified_ns, state.len))
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

/// A source pass's captured primary signature and auxiliary dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub primary: (i64, i64),
    pub dependencies: Option<Stamp>,
}

impl Snapshot {
    /// Retain both declared inventory and conditional reads with their original identities.
    pub(crate) fn all_reads(&self) -> Option<ReadSet> {
        let records = self.dependencies.as_ref()?.records()?;
        let mut reads = ReadSet::default();
        for entry in records.entries.into_iter().chain(records.reads.into_iter().flatten()) {
            reads.entries.entry(entry.path.clone()).or_insert(entry);
        }
        Some(reads)
    }

    /// Recorded consumer reads, for a resumed consumer to retain reads made by its old prefix.
    pub fn reads(&self) -> Option<ReadSet> {
        let entries = self.dependencies.as_ref()?.records()?.reads?;
        Some(ReadSet {
            entries: entries.into_iter().map(|entry| (entry.path.clone(), entry)).collect(),
            known: true,
        })
    }

    /// Auxiliary inventories and all recorded reads still match, independently of primary growth.
    pub fn dependencies_fresh(&self, current: &Self, probe: &mut Probe) -> bool {
        self.dependencies
            .as_ref()
            .zip(current.dependencies.as_ref())
            .is_some_and(|(old, now)| old.matches(now, probe))
    }

    /// Strict consumer freshness using operation-local metadata probes.
    pub fn fresh(&self, current: &Self, probe: &mut Probe) -> bool {
        self.primary == current.primary
            && self
                .dependencies
                .as_ref()
                .zip(current.dependencies.as_ref())
                .is_some_and(|(old, now)| old.same_primary(now))
            && self.dependencies_fresh(current, probe)
    }

    /// Attach the read set captured by this consumer's complete pass, never another consumer's.
    pub fn with_reads(&self, reads: &ReadSet) -> Self {
        let dependencies = (|| {
            if !reads.known {
                return None;
            }
            let mut records = self.dependencies.as_ref()?.records()?;
            records.reads = Some(reads.entries.values().cloned().collect());
            Stamp::serialize(&records)
        })();
        Self {
            primary: self.primary,
            dependencies,
        }
    }
}

/// File and immediate-directory probes shared within one operation. It never descends recursively.
#[derive(Default)]
pub struct Probe {
    // The map key owns the path once; outer None is unavailable, inner None is absent.
    entries: HashMap<PathBuf, Option<Option<FileState>>>,
    directories: HashMap<PathBuf, Option<std::sync::Arc<Vec<PathBuf>>>>,
    // Adapters resolved once per operation: detection stats roots, so not once per session.
    adapters: HashMap<Harness, Option<Box<dyn crate::harness::Adapter>>>,
}

impl Probe {
    fn entry(&mut self, path: &Path) -> Option<Entry> {
        let path = absolute(path)?;
        let state = self
            .entries
            .entry(path.clone())
            .or_insert_with(|| match std::fs::metadata(&path) {
                Ok(md) => {
                    let modified = md
                        .modified()
                        .ok()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_nanos() as u64;
                    #[cfg(unix)]
                    let identity = {
                        use std::os::unix::fs::MetadataExt;
                        Some((md.dev(), md.ino(), md.ctime(), md.ctime_nsec()))
                    };
                    #[cfg(not(unix))]
                    let identity = None;
                    Some(Some(FileState {
                        len: md.len(),
                        modified_ns: modified,
                        identity,
                    }))
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Some(None)
                }
                Err(_) => None,
            })
            .clone()?;
        Some(Entry { path, state })
    }

    fn directory(&mut self, path: &Path) -> Option<std::sync::Arc<Vec<PathBuf>>> {
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

    /// Collect the currently declared session-local dependencies. Plain file transcripts have
    /// an empty set; shared databases depend on WAL and rollback journals even when absent.
    pub fn dependencies(&mut self, r: &SessionRef) -> Option<Stamp> {
        let primary = self.primary_path(r);
        self.dependencies_from(r, &primary)
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
            version: 3,
            primary: self.entry(primary)?,
            entries: self.dependency_entries(r, primary)?,
            reads: None,
        })
    }

    // Publication already has typed captured records; collect fresh entries without a second
    // serialized stamp and parser allocation. The same inventory serves serialized checkpoints.
    fn dependency_entries(&mut self, r: &SessionRef, primary: &Path) -> Option<Vec<Entry>> {
        let mut files = Vec::new();
        let mut directories = Vec::new();
        let ext = r.path.extension().and_then(|e| e.to_str());
        let sqlite = is_sqlite_source(r)?;
        if sqlite {
            files.extend(sqlite_sidecars(&r.path));
        }
        if let Some(metadata) = cursor_cli_metadata(r) {
            files.push(metadata);
        }
        match r.harness {
            Harness::Claude => {
                if r.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-"))
                {
                    files.push(r.path.with_extension("meta.json"));
                }
                directories.push(r.path.with_extension("").join("tool-results"));
                if let Some(session) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "subagents" || n == "workflows"))
                    .and_then(Path::parent)
                {
                    directories.push(session.join("tool-results"));
                }
                if let Some(config) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "projects"))
                    .and_then(Path::parent)
                {
                    directories.push(config.join("tasks").join(&r.id));
                }
            }
            Harness::Grok => {
                files.extend(
                    [
                        "summary.json",
                        "chat_history.jsonl",
                        "updates.jsonl",
                        "system_prompt.txt",
                        "plan.json",
                        "rewind_points.jsonl",
                    ]
                    .map(|name| r.path.join(name)),
                );
                // Only subagents consult sibling parent metadata. Ordinary sessions never scan
                // a cwd group's siblings merely to establish freshness.
                let summary_path = r.path.join("summary.json");
                // Retain the before-read metadata: interpreting kind must not bless a later
                // version of the summary when the inventory is assembled below.
                self.entry(&summary_path)?;
                let subagent = crate::json::JsonSource::capture(&summary_path)
                    .ok()?
                    .read_document(|view| {
                        let kind = view
                            .get("session_kind")?
                            .map(|kind| crate::projection::string_prefix(kind, 9))
                            .transpose()?
                            .flatten();
                        Ok(kind.as_deref() == Some("subagent"))
                    })
                    .ok()?;
                if subagent {
                    let group = r.path.parent()?;
                    files.push(group.to_path_buf());
                    for sibling in self.directory(group)?.iter() {
                        files.push(sibling.join("subagents").join(&r.id).join("meta.json"));
                    }
                }
            }
            Harness::Cline
                if r.path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".messages.json")) =>
            {
                let sid = r.id.split('/').next().unwrap_or(&r.id);
                if let Some(dir) = r.path.parent() {
                    files.push(dir.join(format!("{sid}.json")));
                    if !r.id.contains('/') {
                        if let Some(data) = dir.parent().and_then(Path::parent) {
                            for name in ["sessions.db", "cron.db", "teams.db"] {
                                let db = data.join("db").join(name);
                                files.extend(sqlite_sidecars(&db));
                                files.push(db);
                            }
                        }
                    }
                }
            }
            Harness::Gemini => {
                if let Some(project) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "chats"))
                    .and_then(Path::parent)
                {
                    directories.push(project.join(&r.id).join("tracker"));
                    directories.push(project.join(&r.id).join("plans"));
                }
            }
            Harness::Qwen => {
                files.push(r.path.with_extension("meta.json"));
                if let Some(chats) = r.path.parent().filter(|p| p.file_name().is_some_and(|n| n == "chats")) {
                    if let (Some(root), Some(cwd)) = (
                        chats.parent().and_then(Path::parent).and_then(Path::parent),
                        r.cwd.as_ref().and_then(|p| p.to_str()),
                    ) {
                        files.push(
                            root.join("tmp")
                                .join(crate::digest::sha256_hex(cwd.as_bytes()))
                                .join("scheduled_tasks.json"),
                        );
                    }
                }
            }
            Harness::Cline | Harness::Roo => files.extend(
                [
                    "api_conversation_history.json",
                    "ui_messages.json",
                    "task_metadata.json",
                    "history_item.json",
                ]
                .map(|name| r.path.join(name)),
            ),
            Harness::Kimi => {
                if let Some(root) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "sessions"))
                    .and_then(Path::parent)
                {
                    files.push(root.join("config.toml"));
                }
                files.push(r.path.join("meta.json"));
                if self.entry(&r.path.join("context.jsonl"))?.state.is_some() {
                    // Includes rotated segments and their membership; no subagent/background recursion.
                    directories.push(r.path.clone());
                } else {
                    files.push(r.path.join(format!("{}.jsonl", r.id)));
                }
            }
            Harness::KimiCode => {
                let (session, agent) = match (r.path.parent(), r.path.file_name().and_then(|n| n.to_str())) {
                    (Some(agents), Some(agent))
                        if agents.file_name().is_some_and(|n| n == "agents") && agent.starts_with("agent-") =>
                    {
                        (agents.parent().unwrap_or(&r.path), agent)
                    }
                    _ => (r.path.as_path(), "main"),
                };
                files.push(session.join("state.json"));
                files.push(session.join("agents").join(agent).join("wire.jsonl"));
            }
            Harness::OpenClaw => {
                if let Some(state) = r.path.ancestors().nth(4) {
                    if ext.is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3")) {
                        let db = state.join("state/openclaw.sqlite");
                        files.extend(sqlite_sidecars(&db));
                        files.push(db);
                    } else {
                        if let Some(dir) = r.path.parent() {
                            files.push(dir.join("sessions.json"));
                        }
                        files.push(state.join("cron/jobs.json"));
                        files.push(state.join("subagents/runs.json"));
                    }
                }
            }
            Harness::OpenCode if !sqlite => {
                let storage = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "session"))
                    .and_then(Path::parent)?;
                {
                    let messages = storage.join("message").join(&r.id);
                    // Parts are keyed by the parsed message ID. Read only the ID (other fields are
                    // ignored), and enumerate only this session's own message/part directories.
                    for message in self.directory(&messages)?.iter() {
                        #[derive(Deserialize)]
                        struct MessageId {
                            #[serde(default)]
                            id: String,
                        }
                        let file = std::fs::File::open(message).ok()?;
                        let id: MessageId = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
                        directories.push(storage.join("part").join(id.id));
                    }
                    directories.push(messages);
                }
            }
            _ => {}
        }
        let mut entries = BTreeMap::new();
        for dir in directories {
            entries.insert(dir.clone(), self.entry(&dir)?.state);
            for file in self.directory(&dir)?.iter() {
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
                Some(Entry { path, state })
            })
            .collect()
    }

    pub fn snapshot(&mut self, r: &SessionRef) -> Snapshot {
        let primary = self.primary_path(r);
        self.snapshot_from(r, &primary)
    }

    /// Capture the concrete adapter's append source while retaining its logical session key
    /// and all other auxiliary inputs. Path selection must not perform unrecorded source reads.
    pub fn snapshot_from(&mut self, r: &SessionRef, primary: &Path) -> Snapshot {
        Snapshot {
            primary: self
                .entry(primary)
                .and_then(|entry| entry.state)
                .map_or((0, 0), |state| (state.modified_ns as i64, state.len as i64)),
            dependencies: self.dependencies_from(r, primary),
        }
    }
}

/// The built-in adapter's actual append file, which may differ from a directory session key.
pub fn primary_path(r: &SessionRef) -> PathBuf {
    crate::harness::for_harness(r.harness).map_or_else(|| r.path.clone(), |adapter| adapter.resume_source(r))
}

/// A captured source still matches at publication time. Errors never establish freshness.
pub fn unchanged(r: &SessionRef, snapshot: &Snapshot) -> bool {
    unchanged_from(r, &primary_path(r), snapshot)
}

/// Publication guard against the concrete adapter's source path.
pub fn unchanged_from(r: &SessionRef, primary: &Path, snapshot: &Snapshot) -> bool {
    let mut probe = Probe::default();
    let current_entry = probe.entry(primary);
    let current_primary = current_entry
        .as_ref()
        .and_then(|entry| entry.state.as_ref())
        .map_or((0, 0), |state| (state.modified_ns as i64, state.len as i64));
    if current_primary != snapshot.primary {
        return false;
    }
    let Some(stamp) = snapshot.dependencies.as_ref() else {
        return false;
    };
    let Some(now) = probe.dependency_entries(r, primary) else {
        return false;
    };
    // Directory enumeration is complete. Only metadata probes are needed for dynamic reads;
    // release path lists before parsing the captured inventory's owned paths.
    probe.directories.clear();
    let Some(Records {
        primary: captured_primary,
        entries,
        reads,
        ..
    }) = stamp.records()
    else {
        return false;
    };
    if current_entry.as_ref() != Some(&captured_primary) {
        return false;
    }
    let same = entries == now;
    drop(entries);
    drop(now);
    same && reads.as_ref().is_none_or(|reads| {
        reads
            .iter()
            .all(|entry| probe.entry(&entry.path).as_ref() == Some(entry))
    })
}

pub type Update = (SessionRef, Snapshot);

type Key = (String, String, String);
fn key(r: &SessionRef) -> Key {
    (
        r.harness.as_str().into(),
        r.id.clone(),
        r.path.to_string_lossy().into_owned(),
    )
}

/// A consumer's baselines, loaded once. Older records/missing read sets reread once; even empty
/// complete read sets have a row, so an old consumer cannot silently certify unresolved paths.
#[derive(Default)]
pub struct Table {
    rows: HashMap<Key, ((i64, i64), Stamp)>,
}

impl Table {
    pub fn fresh(&self, r: &SessionRef, primary: (i64, i64), dependencies: &Option<Stamp>) -> bool {
        self.fresh_with_probe(r, primary, dependencies, &mut Probe::default())
    }

    /// Shared operation probes memoize dynamic files consulted by multiple consumers/sessions.
    pub fn fresh_with_probe(
        &self,
        r: &SessionRef,
        primary: (i64, i64),
        dependencies: &Option<Stamp>,
        probe: &mut Probe,
    ) -> bool {
        dependencies.as_ref().is_some_and(|current| {
            self.rows.get(&key(r)).is_some_and(|(old_primary, old)| {
                *old_primary == primary && old.same_primary(current) && old.matches(current, probe)
            })
        })
    }

    /// Only the primary identity, without rewalking auxiliary records. Combine this with
    /// an already checked auxiliary baseline when deciding whether to skip a complete read.
    pub fn primary_fresh(&self, r: &SessionRef, dependencies: &Option<Stamp>) -> bool {
        dependencies
            .as_ref()
            .is_some_and(|current| self.rows.get(&key(r)).is_some_and(|(_, old)| old.same_primary(current)))
    }

    /// Append-safe auxiliary authority, bound to the primary tuple of this consumer's old
    /// output. Primary growth is intentionally excluded; the adapter/checkpoint verifies reuse.
    pub fn auxiliary_fresh_with_probe(
        &self,
        r: &SessionRef,
        baseline_primary: (i64, i64),
        dependencies: &Option<Stamp>,
        probe: &mut Probe,
    ) -> bool {
        dependencies.as_ref().is_some_and(|current| {
            self.rows
                .get(&key(r))
                .is_some_and(|(old_primary, old)| *old_primary == baseline_primary && old.matches(current, probe))
        })
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
                .prepare("SELECT harness,id,path,mtime,size,stamp FROM source_dependencies WHERE consumer=?1")
                .ok()?;
            let rows = stmt
                .query_map([consumer], |row| {
                    Ok((
                        (row.get(0)?, row.get(1)?, row.get(2)?),
                        ((row.get(3)?, row.get(4)?), row.get::<_, String>(5)?),
                    ))
                })
                .ok()?;
            Some(
                rows.flatten()
                    .map(|(key, (primary, text))| (key, (primary, Stamp(text))))
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
    ensure_table(&conn)?;
    Some(conn)
}

#[cfg(feature = "sqlite")]
fn ensure_table(conn: &rusqlite::Connection) -> Option<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_dependencies(
        consumer TEXT NOT NULL,harness TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,
        mtime INTEGER NOT NULL,size INTEGER NOT NULL,stamp TEXT NOT NULL,
        PRIMARY KEY(consumer,harness,id,path));",
    )
    .ok()
}

/// Validate one SQL baseline with the same stamp rules and shared probe used by batch tables.
/// The consumer row and this baseline must be read from one SQL snapshot by the caller.
#[cfg(feature = "sqlite")]
pub(crate) fn fresh_record(
    old_primary: (i64, i64),
    old_stamp: String,
    primary: (i64, i64),
    dependencies: &Option<Stamp>,
    probe: &mut Probe,
) -> bool {
    old_primary == primary
        && dependencies.as_ref().is_some_and(|stamp| {
            let old = Stamp(old_stamp);
            old.same_primary(stamp) && old.matches(stamp, probe)
        })
}

/// Check one previously recorded baseline without loading every session's records during writes.
#[cfg(feature = "sqlite")]
pub(crate) fn fresh_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    primary: (i64, i64),
    dependencies: &Option<Stamp>,
) -> bool {
    dependencies.as_ref().is_some_and(|stamp| {
        let (h, id, path) = key(r);
        conn.query_row(
            "SELECT mtime,size,stamp FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
            rusqlite::params![consumer, h, id, path],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
        )
        .ok()
        .is_some_and(|(mtime, size, old)| {
            let old = Stamp(old);
            (mtime, size) == primary && old.same_primary(stamp) && old.matches(stamp, &mut Probe::default())
        })
    })
}

/// Publish in the same transaction as SQL consumer output. A failure must roll back both.
#[cfg(feature = "sqlite")]
pub(crate) fn publish_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    snapshot: &Snapshot,
) -> Option<()> {
    ensure_table(conn)?;
    let (h, id, path) = key(r);
    conn.execute(
        "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        rusqlite::params![consumer, h, id, path],
    )
    .ok()?;
    if let Some(stamp) = snapshot
        .dependencies
        .as_ref()
        .filter(|stamp| stamp.records().is_some_and(|records| records.reads.is_some()) && unchanged(r, snapshot))
    {
        conn.execute(
            "INSERT INTO source_dependencies(consumer,harness,id,path,mtime,size,stamp)
            VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![consumer, h, id, path, snapshot.primary.0, snapshot.primary.1, stamp.0],
        )
        .ok()?;
    }
    Some(())
}

/// Move a consumer's baselines to a new name. Rows the new name already has are newer and kept.
#[cfg(feature = "sqlite")]
pub fn rename_consumer(from: &str, to: &str) -> bool {
    let rename = || -> Option<()> {
        let mut conn = open()?;
        let tx = conn.transaction().ok()?;
        tx.execute(
            "INSERT OR IGNORE INTO source_dependencies(consumer,harness,id,path,mtime,size,stamp)
             SELECT ?2,harness,id,path,mtime,size,stamp FROM source_dependencies WHERE consumer=?1",
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

/// Publish after the consumer commits its output. Revalidation makes concurrent edits stale;
/// old baselines must already have been invalidated with [`invalidate`]. SQL consumers instead
/// publish atomically with their output through `publish_in`.
#[cfg(feature = "sqlite")]
pub fn update(consumer: &str, updates: &[Update]) {
    if updates.is_empty() {
        return;
    }
    let Some(mut conn) = open() else { return };
    let Ok(tx) = conn.transaction() else { return };
    for (r, snapshot) in updates {
        if publish_in(&tx, consumer, r, snapshot).is_none() {
            return;
        }
    }
    let _ = tx.commit();
}

#[cfg(not(feature = "sqlite"))]
pub fn update(_consumer: &str, _updates: &[Update]) {}

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
        assert!(Probe::default().snapshot(&missing).dependencies.is_none());
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
            let stamp = super::Probe::default().dependencies(&reference);
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
            assert!(Table::load(consumer).fresh(&r, before.primary, &before.dependencies));
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
        assert_eq!(before.primary, now.primary);
        assert!(!before.fresh(&now, &mut Probe::default()));
        assert!(!unchanged(&r, &before));
        assert!(
            before.dependencies_fresh(&now, &mut Probe::default()),
            "auxiliary prefix authority excludes primary identity"
        );
        for consumer in ["counts", "events", "fts", "semantic"] {
            let table = Table::load(consumer);
            assert!(!table.fresh(&r, now.primary, &now.dependencies));
            assert!(table.auxiliary_fresh_with_probe(&r, before.primary, &now.dependencies, &mut Probe::default()));
            assert!(!table.primary_fresh(&r, &now.dependencies));
        }
        // A publication racing the restored-tuple write cannot establish freshness.
        update("counts", &[(r.clone(), before.clone())]);
        assert!(!Table::load("counts").fresh(&r, now.primary, &now.dependencies));
        let now = now.with_reads(&ReadSet::default());
        update("counts", &[(r.clone(), now.clone())]);
        assert!(Table::load("counts").fresh(&r, now.primary, &now.dependencies));
        assert!(!Table::load("events").fresh(&r, now.primary, &now.dependencies));
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
        assert_eq!(now.primary, replaced.primary);
        assert!(!now.fresh(&replaced, &mut Probe::default()));
        assert!(!unchanged(&r, &now));
        assert!(!Table::load("counts").fresh(&r, replaced.primary, &replaced.dependencies));
    }

    #[test]
    fn primary_identity_legacy_and_malformed_records_miss_without_decoding_inventory() {
        let w = crate::test_world("primary-legacy");
        let path = w.base.join("primary.jsonl");
        std::fs::write(&path, "body").unwrap();
        let r = SessionRef::new("s", Harness::Claude, &path);
        let now = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let stamp = now.dependencies.as_ref().unwrap();
        assert!(stamp.same_primary(stamp));
        assert!(stamp.matches(stamp, &mut Probe::default()));
        let mut legacy = serde_json::to_value(stamp.records().unwrap()).unwrap();
        legacy["version"] = 2.into();
        legacy.as_object_mut().unwrap().remove("primary");
        let legacy = Stamp(serde_json::to_string(&legacy).unwrap());
        assert!(!legacy.same_primary(stamp));
        assert!(!legacy.matches(stamp, &mut Probe::default()));
        let malformed = Stamp(stamp.0.replace("\"primary\":{", "\"primary\":null,\"ignored\":{"));
        assert!(!malformed.same_primary(stamp));
        assert!(!malformed.matches(stamp, &mut Probe::default()));
    }

    #[test]
    fn sqlite_journal_creation_rewrite_and_removal_invalidate_inventory() {
        let world = crate::test_world("source-sqlite-journal");
        let db = world.base.join("sessions.db");
        std::fs::write(&db, "fixture").unwrap();
        let journal = world.base.join("sessions.db-journal");
        let r = SessionRef::new("s", Harness::Hermes, &db);
        let absent = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let records = absent.dependencies.as_ref().unwrap().records().unwrap();
        assert!(records
            .entries
            .iter()
            .any(|entry| entry.path == journal && entry.state.is_none()));
        let mut legacy_records = records;
        legacy_records.entries.retain(|entry| entry.path != journal);
        let legacy = Snapshot {
            dependencies: Stamp::serialize(&legacy_records),
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
        let stamp = before.dependencies.as_ref().unwrap();
        assert_eq!(stamp.0, serde_json::to_string(&stamp.records().unwrap()).unwrap());
        assert_eq!(stamp.0.capacity(), stamp.0.len());
        let reference = |captured: &Snapshot| {
            let mut probe = Probe::default();
            let now = probe.snapshot(&r);
            let old = captured.dependencies.as_ref().unwrap().records().unwrap();
            let fresh = now.dependencies.as_ref().unwrap().records().unwrap();
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
        assert_eq!(before.primary, crate::offsets::file_sig(&wire));
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
        let restored: super::Snapshot = serde_json::from_slice(&serde_json::to_vec(&captured).unwrap()).unwrap();
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
        assert!(now.with_reads(&suffix).dependencies.is_none());
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
            assert!(Table::load("relative").fresh(&r, base.primary, &base.dependencies));
            // A different same-named file in the new cwd is irrelevant to the original read.
            std::fs::write("external.txt", "unrelated").unwrap();
            assert!(Table::load("relative").fresh(&r, base.primary, &base.dependencies));
            std::fs::write(root.join("external.txt"), "after!").unwrap();
            assert!(!unchanged(&r, &captured));
            assert!(!Table::load("relative").fresh(&r, base.primary, &base.dependencies));
            return;
        }
        let w = crate::test_world("relative-source-reads");
        std::fs::write(w.base.join("primary.jsonl"), "primary").unwrap();
        std::fs::write(w.base.join("external.txt"), "before").unwrap();
        std::fs::create_dir(w.base.join("other")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "source::tests::relative_reads_remain_bound_to_the_capture_directory",
                "--nocapture",
            ])
            .env(CHILD_ROOT, &w.base)
            .current_dir(&w.base)
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
        assert!(Table::load("dynamic").fresh(&r, base.primary, &base.dependencies));
        std::fs::write(&extra, "before").unwrap();
        reads.record(&extra); // Keep the first capture, even after the dependent read changed.
        assert!(!unchanged(&r, &base.with_reads(&reads)));
        assert!(!Table::load("dynamic").fresh(&r, base.primary, &base.dependencies));
        update("dynamic", &[(r.clone(), before)]);
        assert!(!Table::load("dynamic").fresh(&r, base.primary, &base.dependencies));
        let mut reads = ReadSet::default();
        reads.record(&extra);
        update("dynamic", &[(r.clone(), base.with_reads(&reads))]);
        assert!(Table::load("dynamic").fresh(&r, base.primary, &base.dependencies));
        std::fs::write(&extra, "after!").unwrap();
        assert!(!Table::load("dynamic").fresh(&r, base.primary, &base.dependencies));
        assert_eq!(Probe::default().snapshot(&r).primary, base.primary);
        assert!(!Table::load("other").fresh(&r, base.primary, &base.dependencies));
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
        assert!(!Table::load("upgrade").fresh(&r, base.primary, &base.dependencies));
        let complete = base.with_reads(&ReadSet::default());
        update("upgrade", &[(r.clone(), complete.clone())]);
        assert!(Table::load("upgrade").fresh(&r, base.primary, &base.dependencies));
        let mut table = Table::load("upgrade");
        for (_, stamp) in table.rows.values_mut() {
            stamp.0 = stamp.0.replacen("\"version\":3", "\"version\":1", 1);
        }
        assert!(!table.fresh(&r, base.primary, &base.dependencies));
        assert!(Table::load("upgrade").fresh(&r, base.primary, &base.dependencies));
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
            probe.snapshot(&other).with_reads(&ReadSet::default()).dependencies,
            before.dependencies
        );
        assert_eq!(
            probe.entries.len(),
            3,
            "one main DB and absent WAL/journal probes shared by both sessions"
        );
        assert!(!Table::load("counts").fresh(&r, before.primary, &before.dependencies));
        update("counts", &[(r.clone(), before.clone())]);
        assert!(Table::load("counts").fresh(&r, before.primary, &before.dependencies));
        assert!(!Table::load("events").fresh(&r, before.primary, &before.dependencies));

        std::fs::write(w.base.join("sessions.db-wal"), "new WAL").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(after.primary, before.primary);
        assert_ne!(after.dependencies, before.dependencies);
        assert!(!unchanged(&r, &before));
        update("counts", &[(r.clone(), before)]);
        assert!(
            !Table::load("counts").fresh(&r, after.primary, &after.dependencies),
            "a racing change cannot publish freshness"
        );
        update("counts", &[(r.clone(), after.clone())]);
        assert!(Table::load("counts").fresh(&r, after.primary, &after.dependencies));
        assert!(!Table::load("events").fresh(&r, after.primary, &after.dependencies));
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
            after.dependencies,
            Probe::default()
                .snapshot(&r)
                .with_reads(&ReadSet::default())
                .dependencies
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
        assert_ne!(before.dependencies, after.dependencies, "part-only same-size edit");
        std::fs::write(storage.join("part/mid/new.json"), "new").unwrap();
        assert_ne!(
            after.dependencies,
            Probe::default()
                .snapshot(&r)
                .with_reads(&ReadSet::default())
                .dependencies,
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
            before.dependencies,
            Probe::default()
                .snapshot(&r)
                .with_reads(&ReadSet::default())
                .dependencies
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
