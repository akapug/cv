//! Small verified continuations for repeated API reads. No messages are cached.
use super::{session_meta_json, WindowSink};
use crate::source::{Baseline, Probe, ReadSet};
use crate::stream::{Checkpoint, Flow, ParseOptions};
use crate::{Adapter, Session, SessionRef};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

const MAX_ENTRIES: usize = 32;
const MAX_ENTRY_BYTES: usize = 256 * 1024;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize)]
struct Key {
    harness: String,
    id: String,
    path: std::path::PathBuf,
    primary: std::path::PathBuf,
    version: String,
    extra: bool,
    cwd: Option<std::path::PathBuf>,
    title: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    message_count: usize,
}

impl Key {
    fn of(r: &SessionRef, primary: &std::path::Path, version: String, extra: bool) -> Self {
        Self {
            harness: r.harness.as_str().into(),
            id: r.id.clone(),
            path: r.path.clone(),
            primary: primary.to_path_buf(),
            version,
            extra,
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            message_count: r.message_count,
        }
    }

    fn admitted(r: &SessionRef, primary: &std::path::Path, version: String, extra: bool) -> Option<Self> {
        // Readers can use every catalog hint as fallback metadata. Check the borrowed ref before
        // copying it so oversized demanded hints never become temporary cache keys.
        bounded_size(&(r, primary))?;
        Some(Self::of(r, primary, version, extra))
    }
}

#[derive(Clone, Serialize)]
enum Payload {
    Window {
        index: usize,
        checkpoint: Checkpoint,
        meta: Option<Value>,
    },
    Head(Value),
}

#[derive(Clone, Serialize)]
struct Entry {
    key: Key,
    source: Baseline,
    payload: Payload,
    #[serde(skip)]
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: Mutex<Vec<Entry>>,
}

impl Cache {
    fn get(&self, key: &Key) -> Option<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|entry| &entry.key == key)
            .cloned()
    }
    fn remove(&self, key: &Key) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| &entry.key != key);
    }
    fn insert(&self, mut entry: Entry) {
        let Some(bytes) = bounded_size(&entry) else { return };
        entry.bytes = bytes;
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|old| old.key != entry.key);
        while entries.len() >= MAX_ENTRIES
            || entries.iter().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_CACHE_BYTES
        {
            entries.remove(0);
        }
        entries.push(entry);
    }
}

/// Count serialization through a bounded writer; never allocate a checkpoint-sized JSON string.
pub(super) fn bounded_size(value: &impl Serialize) -> Option<usize> {
    struct Size(usize);
    impl std::io::Write for Size {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_ENTRY_BYTES {
                return Err(std::io::Error::other("API read state exceeds budget"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut size = Size(0);
    serde_json::to_writer(&mut size, value).ok()?;
    Some(size.0)
}

pub(super) fn shared() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

pub(super) fn note_message_sources(message: &crate::Message, primary: &std::path::Path, reads: &mut ReadSet) {
    crate::source::record_message_reads(message, primary, reads);
}

fn captured(r: &SessionRef, primary: &std::path::Path) -> (Baseline, ReadSet) {
    let before = Probe::default().snapshot_from(r, primary);
    let mut reads = ReadSet::default();
    // Include replacement identity and ctime, even when size and mtime were restored.
    reads.record(primary);
    (before, reads)
}

fn fresh(r: &SessionRef, before: &Baseline, primary: &std::path::Path) -> bool {
    let mut probe = Probe::default();
    let current = probe.snapshot_from(r, primary);
    before.fresh(&current, &mut probe)
}

pub(super) fn window(
    cache: &Cache,
    adapter: &dyn Adapter,
    r: &SessionRef,
    start: usize,
    end: Option<usize>,
    extra: bool,
) -> Result<(Session, WindowSink)> {
    let opts = ParseOptions {
        extra,
        ..ParseOptions::lazy()
    };
    let version = crate::resume::version(adapter, "api-window1");
    let primary = adapter.resume_source(r);
    if version.is_none() {
        let (before, reads) = captured(r, &primary);
        let mut sink = WindowSink::page(&primary, start, end, false, reads);
        let shell = adapter.try_stream(r, &opts, &mut sink)?;
        verify(r, &primary, &before, &sink)?;
        return Ok((shell, sink));
    }
    let key = version.and_then(|version| Key::admitted(r, &primary, version, extra));
    let (before, reads) = captured(r, &primary);
    let mut sink = WindowSink::page(&primary, start, end, key.is_some(), reads);
    let mut resumed = false;
    let mut shell = None;
    if let Some(key) = &key {
        if let Some(entry) = cache.get(key) {
            if fresh(r, &entry.source, &primary) {
                if let Payload::Window {
                    index,
                    checkpoint,
                    meta,
                } = entry.payload
                {
                    if index <= start {
                        sink.idx = index;
                        sink.meta = meta;
                        if let Some(old_reads) = entry.source.reads() {
                            sink.reads.merge(&old_reads);
                        }
                        shell = adapter.try_resume(r, &opts, &mut sink, &checkpoint)?;
                        resumed = shell.is_some();
                    }
                }
            } else {
                cache.remove(key);
            }
        }
    }
    if shell.is_none() {
        // A declined resume promises no callbacks, but discard its initial state too.
        let (_, reads) = captured(r, &primary);
        sink = WindowSink::page(&primary, start, end, key.is_some(), reads);
        shell = Some(adapter.try_stream(r, &opts, &mut sink)?);
    }
    let source = before.with_reads(&sink.reads);
    let stable = fresh(r, &source, &primary);
    if resumed && !stable {
        // Do not return a new suffix composed with an old prefix's metadata after a mutation.
        if let Some(key) = &key {
            cache.remove(key);
        }
        let (retry_before, reads) = captured(r, &primary);
        let mut whole = WindowSink::page(&primary, start, end, false, reads);
        let shell = adapter.try_stream(r, &opts, &mut whole)?;
        verify(r, &primary, &retry_before, &whole)?;
        return Ok((shell, whole));
    }
    verify(r, &primary, &before, &sink)?;
    if stable {
        if let (Some(key), Some((index, checkpoint, meta))) = (key, sink.checkpoint.take()) {
            cache.insert(Entry {
                key,
                source,
                payload: Payload::Window {
                    index,
                    checkpoint,
                    meta,
                },
                bytes: 0,
            });
        }
    }
    Ok((shell.expect("a stream or accepted resume returned metadata"), sink))
}

fn verify(r: &SessionRef, primary: &std::path::Path, before: &Baseline, sink: &WindowSink) -> Result<()> {
    anyhow::ensure!(
        fresh(r, &before.with_reads(&sink.reads), primary) && sink.reads.fresh(&mut Probe::default()),
        "API window source changed before publication"
    );
    Ok(())
}

pub(super) fn head(cache: &Cache, adapter: &dyn Adapter, r: &SessionRef) -> Result<Value> {
    let primary = adapter.resume_source(r);
    let key = Key::admitted(r, &primary, format!("{}/api-head1", env!("CARGO_PKG_VERSION")), false);
    if let Some(entry) = key.as_ref().and_then(|key| cache.get(key)) {
        if fresh(r, &entry.source, &primary) {
            if let Payload::Head(value) = entry.payload {
                return Ok(value);
            }
        } else {
            cache.remove(key.as_ref().expect("cached head has a key"));
        }
    }
    let (before, reads) = captured(r, &primary);
    struct Counter {
        primary: std::path::PathBuf,
        total: usize,
        reads: ReadSet,
    }
    impl crate::MessageSink for Counter {
        fn message(&mut self, message: crate::Message) -> Flow {
            note_message_sources(&message, &self.primary, &mut self.reads);
            self.total += 1;
            Flow::Continue
        }
        fn dependency(&mut self, path: &std::path::Path) {
            self.reads.record(path);
        }
    }
    let mut counter = Counter {
        primary: primary.clone(),
        total: 0,
        reads,
    };
    let shell = adapter.try_stream(r, &ParseOptions::lazy(), &mut counter)?;
    let mut value = session_meta_json(&shell);
    let map = value.as_object_mut().expect("session metadata is an object");
    map.insert("total".into(), json!(counter.total));
    map.insert("extra".into(), Value::Object(shell.extra));
    let source = before.with_reads(&counter.reads);
    anyhow::ensure!(
        fresh(r, &source, &primary) && counter.reads.fresh(&mut Probe::default()),
        "API head source changed before publication"
    );
    if let Some(key) = key.filter(|_| bounded_size(&value).is_some()) {
        cache.insert(Entry {
            key,
            source,
            payload: Payload::Head(value.clone()),
            bytes: 0,
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Harness, Message, MessageSink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Observed<'a> {
        inner: &'a dyn Adapter,
        messages: AtomicUsize,
        dependency: Option<std::path::PathBuf>,
        rewrite: Mutex<Option<String>>,
        decline: bool,
    }
    impl<'a> Observed<'a> {
        fn new(inner: &'a dyn Adapter) -> Self {
            Self {
                inner,
                messages: AtomicUsize::new(0),
                dependency: None,
                rewrite: Mutex::new(None),
                decline: false,
            }
        }
    }
    struct Observer<'a> {
        sink: &'a mut dyn MessageSink,
        messages: &'a AtomicUsize,
    }
    impl MessageSink for Observer<'_> {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.fetch_add(1, Ordering::Relaxed);
            self.sink.message(message)
        }
        fn meta(&mut self, session: &Session) {
            self.sink.meta(session);
        }
        fn dependency(&mut self, path: &std::path::Path) {
            self.sink.dependency(path);
        }
        fn wants_checkpoint(&mut self, offset: u64) -> bool {
            self.sink.wants_checkpoint(offset)
        }
        fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
            self.sink.checkpoint(checkpoint);
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.sink.take_error()
        }
    }
    impl Adapter for Observed<'_> {
        fn harness(&self) -> Harness {
            self.inner.harness()
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            self.inner.roots()
        }
        fn discover(&self) -> Result<Vec<SessionRef>> {
            self.inner.discover()
        }
        fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            self.inner.try_stream(
                r,
                opts,
                &mut Observer {
                    sink,
                    messages: &self.messages,
                },
            )
        }
        fn resume_format(&self) -> Option<u32> {
            self.inner.resume_format()
        }
        fn resume_source(&self, r: &SessionRef) -> std::path::PathBuf {
            self.inner.resume_source(r)
        }
        fn resume(
            &self,
            r: &SessionRef,
            opts: &ParseOptions,
            sink: &mut dyn MessageSink,
            cp: &Checkpoint,
        ) -> Result<Option<Session>> {
            if self.decline {
                return Ok(None);
            }
            if let Some(body) = self.rewrite.lock().unwrap().take() {
                std::fs::write(&r.path, body)?;
            }
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            self.inner.try_resume(
                r,
                opts,
                &mut Observer {
                    sink,
                    messages: &self.messages,
                },
                cp,
            )
        }
    }
    fn result(shell: &Session, sink: &mut WindowSink) -> Value {
        json!({ "messages": sink.values().unwrap(), "idx": sink.idx, "more": sink.more,
            "meta": sink.meta.clone().unwrap_or_else(|| session_meta_json(shell)) })
    }
    fn oracle(adapter: &dyn Adapter, r: &SessionRef, start: usize, end: Option<usize>, extra: bool) -> Value {
        let mut sink = WindowSink::page(&adapter.resume_source(r), start, end, false, ReadSet::default());
        let shell = adapter
            .try_stream(
                r,
                &ParseOptions {
                    extra,
                    ..ParseOptions::lazy()
                },
                &mut sink,
            )
            .unwrap();
        result(&shell, &mut sink)
    }
    fn fixture(world: &cv_testkit::World, count: usize) -> SessionRef {
        use std::io::Write as _;
        let path = world.transcript("cached-pages");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for i in 0..count {
            writeln!(
                file,
                r#"{{"type":"user","uuid":"m{i}","message":{{"role":"user","content":"hello"}}}}"#
            )
            .unwrap();
        }
        file.flush().unwrap();
        SessionRef::new("cached-pages", Harness::Claude, path)
    }

    #[test]
    fn heads_and_windows_reject_mutation_before_publication() {
        struct Mutating(bool);
        impl Adapter for Mutating {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn resume_format(&self) -> Option<u32> {
                self.0.then_some(1)
            }
            fn stream(&self, r: &SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
                let mut message = Message::new(crate::Role::Assistant);
                message.content.push(crate::Block::Text {
                    text: "selected".into(),
                });
                sink.try_message(message)?;
                std::fs::write(&r.path, b"changed")?;
                Ok(Session::new(r.id.clone(), r.harness))
            }
        }
        let world = crate::test_world("api-staged-mutation");
        let path = world.home.join("source");
        std::fs::write(&path, b"original").unwrap();
        let r = SessionRef::new("mutating", Harness::Claude, path);
        for resumable in [false, true] {
            std::fs::write(&r.path, b"original").unwrap();
            let cache = Cache::default();
            assert!(window(&cache, &Mutating(resumable), &r, 0, Some(1), false).is_err());
            assert!(cache.entries.lock().unwrap().is_empty());
        }
        std::fs::write(&r.path, b"original").unwrap();
        let cache = Cache::default();
        assert!(head(&cache, &Mutating(false), &r).is_err());
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn native_window_cache_matches_whole_pass_oracles() {
        let world = crate::test_world("api_native_windows");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut checked = 0;
        for harness in [
            Harness::Claude,
            Harness::Codex,
            Harness::KimiCode,
            Harness::Grok,
            Harness::Kimi,
        ] {
            let adapter: Box<dyn Adapter> = match harness {
                Harness::Claude => Box::new(crate::harness::claude::Claude::with_config_dir(
                    &world.home.join(".claude"),
                )),
                Harness::Codex => Box::new(crate::harness::codex::Codex::with_home(&world.home.join(".codex"))),
                Harness::Kimi => Box::new(crate::harness::kimi::Kimi::with_home(&world.home.join(".kimi"))),
                Harness::Grok => Box::new(crate::harness::grok::Grok::with_home(&world.home.join(".grok"))),
                _ => Box::new(crate::harness::kimi_code::KimiCode::with_home(
                    &world.home.join(".kimi-code"),
                )),
            };
            for file in walkdir::WalkDir::new(root.join(harness.as_str()))
                .into_iter()
                .filter_map(Result::ok)
            {
                if file.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let path = if harness == Harness::KimiCode {
                    if file.file_name() != "wire.jsonl" {
                        continue;
                    }
                    let agent = file.path().parent().unwrap();
                    if agent.file_name().unwrap() == "main" {
                        agent.parent().unwrap().parent().unwrap()
                    } else {
                        agent
                    }
                } else if harness == Harness::Kimi {
                    if file.file_name() != "context.jsonl" {
                        continue;
                    }
                    file.path().parent().unwrap()
                } else if harness == Harness::Grok {
                    if file.file_name() != "chat_history.jsonl" {
                        continue;
                    }
                    file.path().parent().unwrap()
                } else {
                    file.path()
                };
                let r = SessionRef::new("oracle", harness, path);
                for extra in [false, true] {
                    let cache = Cache::default();
                    for (start, end) in [
                        (0, Some(2)),
                        (2, Some(4)),
                        (4, Some(6)),
                        (0, Some(1)),
                        (1000, Some(1000)),
                        (1, None),
                    ] {
                        let expected = oracle(adapter.as_ref(), &r, start, end, extra);
                        let (shell, mut sink) = window(&cache, adapter.as_ref(), &r, start, end, extra).unwrap();
                        assert_eq!(
                            result(&shell, &mut sink),
                            expected,
                            "{} {start} {end:?} extra={extra}",
                            file.path().display()
                        );
                    }
                }
                checked += 1;
            }
        }
        assert!(
            checked >= 30,
            "expected frozen Claude/Codex/Kimi-Code native JSONL fixtures, found {checked}"
        );
    }

    #[test]
    fn pages_resume_and_heads_reuse_only_unchanged_sources() {
        let world = crate::test_world("api_cache_invalidation");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in [0, 5, 10] {
            let expected = oracle(&inner, &r, start, Some(start + 5), false);
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 5), false).unwrap();
            assert_eq!(result(&shell, &mut sink), expected);
        }
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 18); // Five selected plus one probe per page.
        let first = head(&cache, &adapter, &r).unwrap();
        let counted = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        assert_eq!(adapter.messages.load(Ordering::Relaxed), counted);
        // An append invalidates both count and continuation rather than assuming prefix immutability.
        use std::io::Write as _;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            r#"{{"type":"user","uuid":"new","message":{{"role":"user","content":"added"}}}}"#
        )
        .unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 21);
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 15, Some(20), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 15, Some(20), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 21);
        // Switching extra flavor must read whole rather than reuse a checkpoint built lean.
        let before = adapter.messages.load(Ordering::Relaxed);
        window(&cache, &adapter, &r, 20, Some(21), true).unwrap();
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 21);
    }

    #[test]
    fn catalog_fallback_hints_are_part_of_read_identity() {
        struct Hints(AtomicUsize);
        impl Adapter for Hints {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(&self, r: &SessionRef, _: &ParseOptions, _: &mut dyn MessageSink) -> Result<Session> {
                self.0.fetch_add(1, Ordering::Relaxed);
                let mut shell = Session::new(&r.id, r.harness);
                shell.title = r.title.clone();
                shell.cwd = r.cwd.clone();
                shell.created_at = r.created_at;
                shell.updated_at = r.updated_at;
                shell.extra.insert("catalog_count".into(), json!(r.message_count));
                shell.extra.insert("created_hint".into(), json!(r.created_at));
                shell.extra.insert("updated_hint".into(), json!(r.updated_at));
                Ok(shell)
            }
        }
        let world = crate::test_world("api_catalog_hint_identity");
        let mut r = fixture(&world, 1);
        let adapter = Hints(AtomicUsize::new(0));
        let cache = Cache::default();
        for i in 0..6 {
            match i {
                1 => r.title = Some("new catalog title".into()),
                2 => r.cwd = Some(world.home.join("new-cwd")),
                3 => r.created_at = Some("2026-01-01T00:00:00Z".parse().unwrap()),
                4 => r.updated_at = Some("2026-02-01T00:00:00Z".parse().unwrap()),
                5 => r.message_count = 42,
                _ => {}
            }
            let value = head(&cache, &adapter, &r).unwrap();
            assert_eq!(value["title"], json!(r.title));
            assert_eq!(value["cwd"], json!(r.cwd));
            assert_eq!(value["extra"]["created_hint"], json!(r.created_at));
            assert_eq!(value["extra"]["updated_hint"], json!(r.updated_at));
            assert_eq!(value["extra"]["catalog_count"], r.message_count);
            assert_eq!(head(&cache, &adapter, &r).unwrap(), value);
            assert_eq!(adapter.0.load(Ordering::Relaxed), i + 1);
        }
        r.title = Some("x".repeat(MAX_ENTRY_BYTES + 1024));
        assert!(Key::admitted(&r, &r.path, "head".into(), false).is_none());
        let prior = cache.entries.lock().unwrap().len();
        for _ in 0..2 {
            assert_eq!(head(&cache, &adapter, &r).unwrap()["title"], json!(r.title));
        }
        assert_eq!(adapter.0.load(Ordering::Relaxed), 8);
        assert_eq!(cache.entries.lock().unwrap().len(), prior);
    }

    #[test]
    fn distinct_actual_sources_with_equal_size_and_mtime_do_not_alias() {
        struct Selected(std::path::PathBuf, AtomicUsize);
        impl Adapter for Selected {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn resume_source(&self, _: &SessionRef) -> std::path::PathBuf {
                self.0.clone()
            }
            fn stream(&self, r: &SessionRef, _: &ParseOptions, _: &mut dyn MessageSink) -> Result<Session> {
                self.1.fetch_add(1, Ordering::Relaxed);
                let mut s = Session::new(&r.id, r.harness);
                s.title = Some(std::fs::read_to_string(&self.0)?);
                Ok(s)
            }
        }
        let world = crate::test_world("api_distinct_actual_sources");
        let r = fixture(&world, 1);
        let old = world.home.join("old-source");
        let new = world.home.join("new-source");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        let modified = std::fs::metadata(&old).unwrap().modified().unwrap();
        std::fs::File::open(&new)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let first = Selected(old, AtomicUsize::new(0));
        let second = Selected(new, AtomicUsize::new(0));
        let cache = Cache::default();
        assert_eq!(head(&cache, &first, &r).unwrap()["title"], "old");
        assert_eq!(head(&cache, &second, &r).unwrap()["title"], "new");
        assert_eq!(head(&cache, &second, &r).unwrap()["title"], "new");
        assert_eq!(second.1.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cache_retention_has_entry_and_byte_budgets() {
        let world = crate::test_world("api_cache_budget");
        let r = fixture(&world, 1);
        let (source, reads) = captured(&r, &crate::source::primary_path(&r));
        let source = source.with_reads(&reads);
        let cache = Cache::default();
        for i in 0..100 {
            cache.insert(Entry {
                key: Key::of(&r, &r.path, format!("head{i}"), false),
                source: source.clone(),
                payload: Payload::Head(json!("x".repeat(100_000))),
                bytes: 0,
            });
        }
        let entries = cache.entries.lock().unwrap();
        assert!(entries.len() <= MAX_ENTRIES);
        assert!(entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_CACHE_BYTES);
        drop(entries);
        assert!(bounded_size(&json!("x".repeat(MAX_ENTRY_BYTES))).is_none());
    }
    #[test]
    fn changed_dynamic_reads_and_declined_resume_fall_back_whole() {
        let world = crate::test_world("api_dependency_invalidation");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let mut adapter = Observed::new(&inner);
        let dependency = world.home.join("absent-helper.json");
        adapter.dependency = Some(dependency.clone());
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        std::fs::write(dependency, "new metadata").unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 11);
        adapter.decline = true;
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 10, Some(15), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 10, Some(15), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 16);
    }

    #[test]
    fn mutation_during_resume_retries_without_stale_prefix_metadata() {
        let world = crate::test_world("api_resume_mutation");
        let r = fixture(&world, 20);
        let old = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"old-title\"}}\n{}",
            std::fs::read_to_string(&r.path).unwrap()
        );
        std::fs::write(&r.path, &old).unwrap();
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        *adapter.rewrite.lock().unwrap() = Some(old.replace("old-title", "new-title"));
        let (shell, mut sink) = window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 5, Some(10), false));
        assert_eq!(shell.title.as_deref(), Some("new-title"));
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn same_length_rewrite_with_restored_mtime_invalidates_primary_read() {
        let world = crate::test_world("api_primary_identity");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        let modified = std::fs::metadata(&r.path).unwrap().modified().unwrap();
        let body = std::fs::read_to_string(&r.path).unwrap().replace("hello", "other");
        std::fs::write(&r.path, body).unwrap();
        std::fs::File::open(&r.path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 5, Some(10), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 11);
    }

    #[test]
    #[ignore = "paired callback proof using a frozen scratch transcript"]
    fn sequential_pages_and_repeated_heads_read_work() {
        let world = crate::test_world("api_cached_read_work");
        let r = fixture(&world, 20_000);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let expected = oracle(&inner, &r, start, Some(start + 200), false);
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(result(&shell, &mut sink), expected);
        }
        let page_callbacks = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(page_callbacks, 20_099);
        for _ in 0..5 {
            assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 20_000);
        }
        let head_callbacks = adapter.messages.load(Ordering::Relaxed) - page_callbacks;
        assert_eq!(head_callbacks, 20_000);
        eprintln!("pages=100 selected=20000 cached_page_callbacks={page_callbacks} repeated_heads=5 head_callbacks={head_callbacks}");
    }
    #[test]
    fn plain_codex_pages_reuse_matching_reader_checkpoints() {
        let world = crate::test_world("api_plain_codex_pages");
        let path = world.base.join("rollout-pages.jsonl");
        let line = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n";
        std::fs::write(&path, line.repeat(20_000)).unwrap();
        let r = SessionRef::new("pages", Harness::Codex, path);
        let inner = crate::harness::codex::Codex::with_home(&world.home.join(".codex"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(sink.selected, 200);
            assert_eq!(sink.idx, (start + 201).min(20_000));
            if start % 2000 == 0 || start == 19800 {
                assert_eq!(
                    result(&shell, &mut sink),
                    oracle(&inner, &r, start, Some(start + 200), false)
                );
            }
        }
        let pages = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(pages, 20_099);
        let first = head(&cache, &adapter, &r).unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        let heads = adapter.messages.load(Ordering::Relaxed) - pages;
        assert_eq!(heads, 20_000);
        eprintln!("codex production pages=100 selected=20000 callbacks={pages} repeated_heads=2 callbacks={heads}");
    }

    #[test]
    fn directory_pages_reuse_actual_wire_and_invalidate_on_wire_or_state_changes() {
        use std::io::Write as _;
        let world = crate::test_world("api_kimi_code_pages");
        let dir = world.home.join(".kimi-code/sessions/wd_test/session_pages");
        std::fs::create_dir_all(dir.join("agents/main")).unwrap();
        let wire = dir.join("agents/main/wire.jsonl");
        let state = dir.join("state.json");
        std::fs::write(&state, r#"{"id":"session_pages","cwd":"/old"}"#).unwrap();
        let line = "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
        std::fs::write(&wire, line.repeat(20_000)).unwrap();
        let r = SessionRef::new("pages", Harness::KimiCode, dir);
        let inner = crate::harness::kimi_code::KimiCode::with_home(&world.home.join(".kimi-code"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let (shell, mut sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(sink.selected, 200);
            assert_eq!(sink.idx, (start + 201).min(20_000));
            if start % 2000 == 0 || start == 19800 {
                assert_eq!(
                    result(&shell, &mut sink),
                    oracle(&inner, &r, start, Some(start + 200), false)
                );
            }
        }
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 20_099);
        let first = head(&cache, &adapter, &r).unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 40_099);
        // Direct changes to the nested source must invalidate a directory-keyed head and page.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 20_001);
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 19900, Some(19905), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 19900, Some(19905), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 19_906);
        std::fs::write(&state, r#"{"id":"session_pages","cwd":"/new"}"#).unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, mut sink) = window(&cache, &adapter, &r, 19905, Some(19910), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&inner, &r, 19905, Some(19910), false));
        assert_eq!(shell.cwd.as_deref(), Some(std::path::Path::new("/new")));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 19_911);
        assert!(cache
            .entries
            .lock()
            .unwrap()
            .iter()
            .all(|entry| entry.bytes < 16 * 1024));
        eprintln!("kimi-code actual API: pages=100 selected=20000 callbacks=20099; two heads callbacks=20000; wire/state changes force exact whole fallback");
    }

    #[test]
    fn source_json_paths_are_dependencies_even_without_adapter_notifications() {
        struct External {
            path: std::path::PathBuf,
            calls: AtomicUsize,
        }
        impl Adapter for External {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(&self, r: &SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                let input = crate::json::Json::source(&self.path, 0, std::fs::metadata(&self.path)?.len())?;
                let mut message = Message::new(crate::Role::Assistant);
                message.content.push(crate::Block::ToolUse {
                    id: "call".into(),
                    name: "Bash".into(),
                    namespace: None,
                    input,
                });
                sink.message(message);
                Ok(Session::new(&r.id, r.harness))
            }
        }
        let world = crate::test_world("api_external_json_dependencies");
        let r = fixture(&world, 1);
        let path = world.home.join("external.json");
        std::fs::write(&path, r#"{"command":"old"}"#).unwrap();
        let adapter = External {
            path: path.clone(),
            calls: AtomicUsize::new(0),
        };
        let cache = Cache::default();
        head(&cache, &adapter, &r).unwrap();
        head(&cache, &adapter, &r).unwrap();
        assert_eq!(adapter.calls.load(Ordering::Relaxed), 1);
        let (before, reads) = captured(&r, &crate::source::primary_path(&r));
        let mut sink = WindowSink::page(&crate::source::primary_path(&r), 1, Some(2), false, reads);
        adapter.try_stream(&r, &ParseOptions::lazy(), &mut sink).unwrap();
        let source = before.with_reads(&sink.reads);
        assert!(fresh(&r, &source, &crate::source::primary_path(&r)));
        std::fs::write(path, r#"{"command":"updated"}"#).unwrap();
        assert!(!fresh(&r, &source, &crate::source::primary_path(&r)));
        head(&cache, &adapter, &r).unwrap();
        assert_eq!(adapter.calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn oversized_demanded_metadata_is_returned_without_cache_retention() {
        let world = crate::test_world("api_large_metadata_budget");
        let r = fixture(&world, 20);
        let title = "x".repeat(MAX_ENTRY_BYTES + 1024);
        let body = format!(
            "{}\n{}",
            json!({"type":"ai-title", "aiTitle":title}),
            std::fs::read_to_string(&r.path).unwrap()
        );
        std::fs::write(&r.path, body).unwrap();
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in [0, 5] {
            let (shell, _) = window(&cache, &adapter, &r, start, Some(start + 5), false).unwrap();
            assert_eq!(shell.title.as_deref(), Some(title.as_str()));
        }
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 17);
        assert_eq!(head(&cache, &adapter, &r).unwrap()["title"], title);
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn discarded_giant_tool_arguments_do_not_enter_continuation_state() {
        let world = crate::test_world("api_discarded_tool_arguments");
        let r = fixture(&world, 20);
        let body = std::fs::read_to_string(&r.path).unwrap();
        let (first, rest) = body.split_once('\n').unwrap();
        let giant = json!({"type":"assistant", "uuid":"giant", "message":{"role":"assistant", "content":[{
            "type":"tool_use", "id":"tool", "name":"Edit", "input":{"file_path":"a.rs", "content":"discarded-tool-content".repeat(100_000)}
        }]}});
        std::fs::write(&r.path, format!("{first}\n{giant}\n{rest}")).unwrap();
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let cache = Cache::default();
        let (shell, mut sink) = window(&cache, &adapter, &r, 2, Some(4), false).unwrap();
        assert_eq!(result(&shell, &mut sink), oracle(&adapter, &r, 2, Some(4), false));
        let entries = cache.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].bytes < 16 * 1024);
        assert!(!serde_json::to_string(&entries[0])
            .unwrap()
            .contains("discarded-tool-content"));
    }
}
