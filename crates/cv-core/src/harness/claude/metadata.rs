//! Discovery reads only titles, cwd, timestamps and whether an assistant is synthetic: the
//! record codec's metadata demand, over the same byte source as every other Claude read.

use super::*;
use crate::harness::store::document::Fields;
use serde::{Deserialize, Serialize};

#[cfg(test)]
thread_local! { pub(super) static FRAMED: std::cell::RefCell<Vec<(u64,u64)>> = const { std::cell::RefCell::new(Vec::new()) }; }

pub(super) const CONTINUATION_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "/claude.discovery/1");
const LAYER: &str = "claude.discovery";

#[derive(Clone, Serialize, Deserialize)]
struct State {
    session: SessionRef,
    custom_title: bool,
}

pub(super) struct Scan {
    session: SessionRef,
    custom_title: bool,
    prefix: Option<(u64, State)>,
}

impl Scan {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            session: SessionRef::new(
                path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
                Harness::Claude,
                path,
            ),
            custom_title: false,
            prefix: None,
        }
    }

    pub(super) fn restore(path: &Path, checkpoint: Option<&Checkpoint>) -> (Self, u64) {
        let mut scan = Self::new(path);
        if let Some(cp) = checkpoint {
            if let Some(state) = cp.get::<State>(LAYER) {
                if state.session.id == scan.session.id
                    && state.session.path == scan.session.path
                    && state.session.harness == Harness::Claude
                    && state.session.message_count as u64 <= cp.offset
                {
                    scan.session = state.session;
                    scan.custom_title = state.custom_title;
                    return (scan, cp.offset);
                }
            }
        }
        (scan, 0)
    }

    fn save_prefix(&mut self, offset: u64) {
        self.prefix = Some((
            offset,
            State {
                session: self.session.clone(),
                custom_title: self.custom_title,
            },
        ));
    }

    pub(super) fn checkpoint(&self) -> Option<Checkpoint> {
        let (offset, state) = self.prefix.as_ref()?;
        let mut checkpoint = Checkpoint::new(*offset);
        checkpoint.put(LAYER, state);
        Some(checkpoint)
    }

    /// Fold lines `offset..` of the captured transcript. The continuation is the state after the
    /// last finished line; a partial last line is folded but re-read next time.
    pub(super) fn read(&mut self, source: &JsonSource, file: &fs::File, offset: u64) -> Result<()> {
        let mut finished = offset;
        let mut partial = false;
        crate::harness::store::jsonl::for_each_line(source, file, offset, source.len(), |line| {
            if !line.complete {
                self.save_prefix(finished);
                partial = true;
            }
            if let Some(view) = line.view {
                self.record(view)?;
            }
            if line.complete {
                finished = line.end;
            }
            Ok(Flow::Continue)
        })?;
        if !partial {
            self.save_prefix(finished);
        }
        Ok(())
    }

    fn record(&mut self, view: crate::json::JsonView<'_>) -> Result<()> {
        let Some(fields) = Fields::parse(
            view,
            &[
                "type",
                "cwd",
                "timestamp",
                "customTitle",
                "aiTitle",
                "isApiErrorMessage",
                "message",
            ],
        )?
        else {
            return Ok(());
        };
        let ty = fields.string("type")?.unwrap_or_default();
        if ty == "custom-title" {
            if let Some(title) = fields.string("customTitle")? {
                self.session.title = Some(title);
                self.custom_title = true;
            }
        }
        if ty == "ai-title" && !self.custom_title {
            if let Some(title) = fields.string("aiTitle")? {
                self.session.title = Some(title);
            }
        }
        if self.session.cwd.is_none() {
            self.session.cwd = fields.string("cwd")?.map(PathBuf::from);
        }
        if let Some(ts) = fields.string("timestamp")?.as_deref().and_then(parse_ts) {
            self.session.created_at = Some(self.session.created_at.map_or(ts, |old| old.min(ts)));
            self.session.updated_at = Some(self.session.updated_at.map_or(ts, |old| old.max(ts)));
        }
        if matches!(ty.as_str(), "user" | "assistant") {
            let synthetic = ty == "assistant"
                && (fields
                    .get("isApiErrorMessage")
                    .map(|v| v.as_bool())
                    .transpose()?
                    .flatten()
                    == Some(true)
                    || match fields
                        .get("message")
                        .map(|m| Fields::parse(m, &["model"]))
                        .transpose()?
                        .flatten()
                    {
                        Some(message) => message.string("model")?.as_deref() == Some(SYNTHETIC_MODEL),
                        None => false,
                    });
            if !synthetic {
                self.session.message_count += 1;
            }
        }
        Ok(())
    }

    pub(super) fn finish(self) -> SessionRef {
        self.session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The former discovery reducer is the oracle: its count deliberately does not parse content,
    // and its cwd/timestamps include metadata-only records unlike the conversational parser.
    fn owned(path: &Path, bytes: &[u8]) -> SessionRef {
        let mut out = Scan::new(path);
        super::super::super::for_each_json_line(std::io::Cursor::new(bytes), |v| {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if ty == "custom-title" {
                if let Some(t) = v.get("customTitle").and_then(Value::as_str) {
                    out.session.title = Some(t.into());
                    out.custom_title = true;
                }
            }
            if ty == "ai-title" && !out.custom_title {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    out.session.title = Some(t.into());
                }
            }
            if out.session.cwd.is_none() {
                out.session.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            }
            if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
                out.session.created_at = Some(out.session.created_at.map_or(ts, |c| c.min(ts)));
                out.session.updated_at = Some(out.session.updated_at.map_or(ts, |u| u.max(ts)));
            }
            let synthetic = ty == "assistant"
                && (v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
                    || v.pointer("/message/model").and_then(Value::as_str) == Some(SYNTHETIC_MODEL));
            if matches!(ty, "user" | "assistant") && !synthetic {
                out.session.message_count += 1;
            }
            Flow::Continue
        });
        out.finish()
    }

    fn check(path: &Path, bytes: &[u8]) {
        let world = crate::test_world("claude-discovery-oracle");
        let file = world.base.join(path.file_name().unwrap());
        fs::write(&file, bytes).unwrap();
        assert_eq!(
            serde_json::to_value(scan(&file).unwrap()).unwrap(),
            serde_json::to_value(owned(&file, bytes)).unwrap(),
            "{}",
            path.display()
        );
    }

    #[test]
    fn continuation_replays_partial_and_valid_unterminated_records() {
        let world = crate::test_world("claude-metadata-resume-tails");
        let path = world.base.join("s.jsonl");
        let prefix = "{\"type\":\"summary\",\"cwd\":\"/metadata\",\"timestamp\":\"2026-02-01T00:00:00Z\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"AI\"}\n";
        let tails = [
            (r#"{"type":"user","message":null}"#, "\n{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"blocked\"}\n"),
            (r#"{"type":"user"}"#, " trailing\n{\"type\":\"assistant\",\"isApiErrorMessage\":true}\n"),
            (r#"{"type":"custom-title","customTitle":"part"#, "ial\"}\n{\"type\":\"user\",\"timestamp\":\"2025-01-01T00:00:00Z\"}\n"),
            (r#"{"type":"user","unknown":"\ud800"}"#, "\n{\"type\":\"assistant\",\"message\":{\"model\":\"normal\",\"model\":\"<synthetic>\"}}\n"),
        ];
        for (tail, appended) in tails {
            let old = format!("{prefix}{tail}");
            fs::write(&path, &old).unwrap();
            let (before, cp) = scan_at(&path, None).unwrap();
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(owned(&path, old.as_bytes())).unwrap()
            );
            let cp = cp.unwrap();
            assert_eq!(cp.offset, prefix.len() as u64);
            let grown = format!("{old}{appended}");
            fs::write(&path, &grown).unwrap();
            FRAMED.with(|frames| frames.borrow_mut().clear());
            let (resumed, next) = scan_at(&path, Some(&cp)).unwrap();
            assert_eq!(
                serde_json::to_value(resumed).unwrap(),
                serde_json::to_value(owned(&path, grown.as_bytes())).unwrap()
            );
            assert_eq!(
                FRAMED.with(|frames| frames.borrow().clone()),
                vec![(cp.offset, (grown.len() - prefix.len()) as u64)]
            );
            assert_eq!(next.unwrap().offset, grown.len() as u64);
            #[cfg(all(feature = "sqlite", unix))]
            {
                use std::io::Write;
                fs::write(&path, &old).unwrap();
                cached_metadata(&path, false).unwrap();
                crate::discover_cache::persist(false);
                let conn = crate::catalog::open_db().unwrap();
                crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(appended.as_bytes())
                    .unwrap();
                FRAMED.with(|frames| frames.borrow_mut().clear());
                assert_eq!(
                    serde_json::to_value(cached_metadata(&path, false).unwrap()).unwrap(),
                    serde_json::to_value(owned(&path, grown.as_bytes())).unwrap()
                );
                assert_eq!(
                    FRAMED.with(|frames| frames.borrow().clone()),
                    vec![(cp.offset, (grown.len() - prefix.len()) as u64)],
                    "persisted continuation must replay the entire provisional EOF record"
                );
            }
        }
    }

    #[cfg(all(feature = "sqlite", unix))]
    #[test]
    fn cached_continuation_is_persisted_and_rejects_rewrites_replacements_and_corruption() {
        use std::io::Write;
        let world = crate::test_world("claude-metadata-cache-resume");
        let path = world.base.join("s.jsonl");
        let prefix = format!("{{\"type\":\"summary\",\"cwd\":\"/metadata\"}}\n{{\"type\":\"assistant\",\"message\":{{\"content\":\"{}\"}}}}\n", "ignored".repeat(32768));
        fs::write(&path, &prefix).unwrap();
        let check = |expected: &str, extent: Option<(u64, u64)>| {
            FRAMED.with(|frames| frames.borrow_mut().clear());
            let actual = cached_metadata(&path, false).unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(owned(&path, expected.as_bytes())).unwrap()
            );
            assert_eq!(
                FRAMED.with(|frames| frames.borrow().clone()),
                extent.into_iter().collect::<Vec<_>>()
            );
        };
        check(&prefix, Some((0, prefix.len() as u64)));
        check(&prefix, None);
        crate::discover_cache::persist(false);
        let conn = crate::catalog::open_db().unwrap();
        let saved: String = conn
            .query_row(
                "SELECT continuation FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(serde_json::from_str::<crate::discover_cache::ScanContinuation>(&saved).is_ok());
        crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
        let appended = "{\"type\":\"custom-title\",\"customTitle\":\"new title\"}\n{\"type\":\"user\",\"timestamp\":\"2025-01-01T00:00:00Z\"}\n";
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(appended.as_bytes())
            .unwrap();
        let mut text = format!("{prefix}{appended}");
        check(&text, Some((prefix.len() as u64, appended.len() as u64)));
        check(&text, None);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        text = text.replace("new title", "bad title");
        fs::write(&path, &text).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        check(&text, Some((0, text.len() as u64)));
        let replacement = world.base.join("replacement");
        let more = "{\"type\":\"ai-title\",\"aiTitle\":\"still blocked\"}\n";
        text.push_str(more);
        fs::write(&replacement, &text).unwrap();
        fs::rename(replacement, &path).unwrap();
        check(&text, Some((0, text.len() as u64)));
        crate::discover_cache::persist(false);
        conn.execute(
            "UPDATE scan_cache SET continuation='broken' WHERE path=?1",
            [path.to_string_lossy()],
        )
        .unwrap();
        crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
        check(&text, None); // An ordinary exact hit does not decode continuation JSON.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"user\"}\n")
            .unwrap();
        text.push_str("{\"type\":\"user\"}\n");
        check(&text, Some((0, text.len() as u64)));
    }

    #[cfg(all(feature = "sqlite", unix))]
    #[test]
    fn cached_checkpoint_is_independent_of_counts_and_rejects_invalid_states() {
        use std::io::Write;
        let world = crate::test_world("claude-metadata-state-guards");
        let path = world.base.join("s.jsonl");
        let mut text = String::from("{\"type\":\"ai-title\",\"aiTitle\":\"initial\"}\n");
        fs::write(&path, &text).unwrap();
        cached_metadata(&path, false).unwrap();
        crate::discover_cache::persist(false);
        let conn = crate::catalog::open_db().unwrap();
        crate::discover_cache::fill_count(&conn, &path, Harness::Claude, "s", 99);
        let append = "{\"type\":\"user\"}\n";
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(append.as_bytes())
            .unwrap();
        text.push_str(append);
        assert_eq!(
            cached_metadata(&path, false).unwrap().message_count,
            1,
            "published fallback counts do not alter reducer state"
        );
        for mutation in 0..8 {
            crate::discover_cache::persist(false);
            let encoded: String = conn
                .query_row(
                    "SELECT continuation FROM scan_cache WHERE path=?1",
                    [path.to_string_lossy()],
                    |row| row.get(0),
                )
                .unwrap();
            let mut state: crate::discover_cache::ScanContinuation = serde_json::from_str(&encoded).unwrap();
            match mutation {
                0 => state.head.version = "obsolete".into(),
                1 => state.checkpoint.offset += 1,
                2 => {
                    state.head.prefix.len = text.len() as u64 + 100;
                    state.checkpoint.offset = state.head.offset();
                }
                3 => state.dependencies = crate::source::ReadSet::unrecorded(),
                4 => state.checkpoint = Checkpoint::new(state.checkpoint.offset),
                5 => state.checkpoint.put(
                    LAYER,
                    &serde_json::json!({"session": "wrong-type", "custom_title": false}),
                ),
                6 => state.head.prefix.ino ^= 1,
                _ => {
                    let mut reducer = state.checkpoint.get::<State>(LAYER).unwrap();
                    reducer.session.message_count = usize::MAX;
                    state.checkpoint.put(LAYER, &reducer);
                }
            }
            conn.execute(
                "UPDATE scan_cache SET continuation=?1 WHERE path=?2",
                rusqlite::params![serde_json::to_string(&state).unwrap(), path.to_string_lossy()],
            )
            .unwrap();
            crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(append.as_bytes())
                .unwrap();
            text.push_str(append);
            FRAMED.with(|frames| frames.borrow_mut().clear());
            assert_eq!(
                serde_json::to_value(cached_metadata(&path, false).unwrap()).unwrap(),
                serde_json::to_value(owned(&path, text.as_bytes())).unwrap()
            );
            assert_eq!(
                FRAMED.with(|frames| frames.borrow().clone()),
                vec![(0, text.len() as u64)],
                "invalid state {mutation} falls back to the whole reducer"
            );
        }
        crate::discover_cache::persist(false);
        conn.execute(
            "UPDATE scan_cache SET continuation=NULL WHERE path=?1",
            [path.to_string_lossy()],
        )
        .unwrap();
        crate::discover_cache::invalidate_interpretation(Path::new(conn.path().unwrap()));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(append.as_bytes())
            .unwrap();
        text.push_str(append);
        FRAMED.with(|frames| frames.borrow_mut().clear());
        cached_metadata(&path, false).unwrap();
        assert_eq!(
            FRAMED.with(|frames| frames.borrow().clone()),
            vec![(0, text.len() as u64)]
        );
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        FRAMED.with(|frames| frames.borrow_mut().clear());
        assert_eq!(cached_metadata(&path, false).unwrap().message_count, 1);
        assert_eq!(
            FRAMED.with(|frames| frames.borrow().clone()),
            vec![(0, append.len() as u64)]
        );
    }

    /// A pass that races its writer still publishes what it read, so each later pass reads only
    /// what was appended since: discovery of a live transcript costs its appended bytes.
    #[cfg(unix)]
    #[test]
    fn discovery_beside_a_live_writer_reads_each_appended_byte_about_once() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        let world = crate::test_world("claude-metadata-live-writer");
        let path = world.base.join("s.jsonl");
        let record = |i: usize| {
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-02-01T00:00:00Z\",\"message\":{{\"content\":\"{}\"}}}}\n",
                format!("turn {i:08} ").repeat(48)
            )
        };
        fs::write(&path, (0..4096).map(record).collect::<String>()).unwrap();
        let stop = AtomicBool::new(false);
        let frames = std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
                for i in 4096.. {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Two writes: readers see a partial last line between them.
                    let line = record(i);
                    let (head, tail) = line.split_at(line.len() / 2);
                    file.write_all(head.as_bytes()).unwrap();
                    file.write_all(tail.as_bytes()).unwrap();
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            });
            // Stops the writer however this closure exits, so a failed pass cannot hang the scope.
            struct Stop<'a>(&'a AtomicBool);
            impl Drop for Stop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _stop = Stop(&stop);
            FRAMED.with(|frames| frames.borrow_mut().clear());
            for _ in 0..8 {
                cached_metadata(&path, false).expect("a growing transcript stays listed");
            }
            FRAMED.with(|frames| frames.borrow().clone())
        });
        assert_eq!(frames.first().map(|frame| frame.0), Some(0));
        assert!(
            frames[1..].iter().all(|&(offset, _)| offset > 0),
            "every later pass resumes: {frames:?}"
        );
        // Passes overlap only by the partial record each one replays.
        let read: u64 = frames.iter().map(|&(_, len)| len).sum();
        let bound = fs::metadata(&path).unwrap().len() + (frames.len() * record(0).len()) as u64;
        assert!(read <= bound, "read {read} bytes for a {bound}-byte bound: {frames:?}");
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            serde_json::to_value(cached_metadata(&path, false).unwrap()).unwrap(),
            serde_json::to_value(owned(&path, &bytes)).unwrap()
        );
    }

    #[test]
    fn discovery_projection_preserves_owned_record_semantics() {
        let mut cases = vec![
            "null".into(),
            "[]".into(),
            "123".into(),
            "\"text\"".into(),
            r#"{"type":"user","message":null}"#.into(),
            r#"{"type":"assistant","message":{"model":"<synthetic>"}}"#.into(),
            r#"{"type":"assistant","isApiErrorMessage":true,"message":null}"#.into(),
            r#"{"type":"assistant","isApiErrorMessage":"true","message":null}"#.into(),
            r#"{"type":"assistant","message":{"model":"<synthetic>"},"message":[]}"#.into(),
            r#"{"type":"assistant","message":{"model":"<synthetic>","model":null}}"#.into(),
            r#"{"type":"user","type":"assistant","isApiErrorMessage":true,"isApiErrorMessage":false}"#.into(),
            r#"{"type":"user","type":{},"cwd":null,"cwd":"/cwd"}"#.into(),
            r#"{"type":"custom-title","customTitle":"a","customTitle":[]}"#.into(),
            r#"{"\u0074ype":"assistant","message":{"model":"\u003csynthetic>"}}"#.into(),
            r#"{"type":"user","cwd":"/a\u0020b","timestamp":"2026-01-01T00:00:00Z"}"#.into(),
            r#"{"type":"user","unknown":1e400,"unknown":0}"#.into(),
            r#"{"type":"user","unknown":"\ud800","unknown":0}"#.into(),
            r#"{"type":"user","unknown":{"\ud800":0}}"#.into(),
            r#"{"type":"user","message":{"model":"normal","unknown":"\udc00"}}"#.into(),
            "{\"type\":\"user\"} trailing".into(),
        ];
        for depth in 125..=129 {
            cases.push(format!(
                "{{\"type\":\"user\",\"unknown\":{}0{}}}",
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for case in &cases {
            check(Path::new("oracle.jsonl"), case.as_bytes());
        }
        cases.extend([
            r#"{"type":"summary","summary":"ignored","cwd":"/metadata","timestamp":"2026-01-02T00:00:00Z"}"#.into(),
            r#"{"type":"ai-title","aiTitle":"AI"}"#.into(),
            r#"{"type":"custom-title","customTitle":null}"#.into(),
            r#"{"type":"ai-title","aiTitle":"new AI","timestamp":"2026-01-01T00:00:00Z"}"#.into(),
            r#"{"type":"custom-title","customTitle":"\u2603"}"#.into(),
            r#"{"type":"custom-title","customTitle":false}"#.into(),
            r#"{"type":"ai-title","aiTitle":"blocked"}"#.into(),
            r#"{"type":"attachment","timestamp":"2026-01-03T00:00:00Z"}"#.into(),
        ]);
        let bytes = cases.join("\r\n");
        check(Path::new("combined.jsonl"), bytes.as_bytes());
        check(
            Path::new("invalid-utf8.jsonl"),
            b"{\"type\":\"user\",\"unknown\":\"\xff\"}\n{\"type\":\"user\"}",
        );
    }

    #[test]
    fn discovery_projection_matches_owned_corpus_and_file_transport() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/claude");
        for entry in WalkDir::new(corpus).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                check(path, &fs::read(path).unwrap());
            }
        }
        let dir = std::env::temp_dir().join(format!("cv-claude-metadata-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let bytes = b"\n{\"type\":\"user\"}\r\n{\"type\":\"assistant\",\"message\":{\"model\":\"<synthetic>\"}}\n{\"type\":\"custom-title\",\"customTitle\":\"tail\"}";
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            serde_json::to_value(scan(&path).unwrap()).unwrap(),
            serde_json::to_value(owned(&path, bytes)).unwrap()
        );
        fs::write(&path, []).unwrap();
        assert_eq!(scan(&path).unwrap().message_count, 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
