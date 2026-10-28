//! Precompaction replayed on real compactions: each fixture under `fixtures/precompact/` is a
//! Claude Code session window and the compaction request the harness built from it, redacted
//! together by `tools/precompact-fixture.py` (equal text stays equal on both sides, so what matched
//! in the real pair matches in the fixture). `expected.json` holds what `cv precompact check` gave
//! on the real, unredacted pair.
//!
//! For each pair: a summary cut at the window's newest message matches the request with the tail
//! the real pair had; a summary cut earlier (at 70% of the window's fill) matches with a longer
//! tail; and an edit to the history under the summary misses.

use cv_core::harness::claude::Claude;
use cv_core::ir::{Harness, SessionRef};
use cv_core::precompact::{validate, Miss, Record, RequestView, WindowRef, RECORD_VERSION};
use cv_core::{Adapter, Demand, Flow, Message, Resolver, TryFnSink};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/precompact")
}

/// The window after each message: `(fill, record cut there)`.
fn replay(path: &Path) -> Vec<(Option<u64>, Record)> {
    let adapter = Claude::with_config_dir(&dir());
    let r = SessionRef::new(path.file_stem().unwrap().to_str().unwrap(), Harness::Claude, path);
    let resolver = Resolver::new(Some(path.to_path_buf()));
    let mut window = cv_core::precompact::Window::default();
    let mut out = Vec::new();
    let mut sink = TryFnSink::new(|m: Message| {
        window.observe(&m, &resolver)?;
        if let Some((cut, anchors)) = window.cut_now() {
            out.push((
                window.fill(),
                Record {
                    v: RECORD_VERSION,
                    harness: "claude".into(),
                    session: r.id.clone(),
                    window: WindowRef {
                        boundaries: window.boundaries.len(),
                        start: window.start,
                    },
                    cut,
                    anchors,
                    profile: String::new(),
                    model: String::new(),
                    created: chrono::Utc::now(),
                    base: None,
                    bounds: Vec::new(),
                    text: String::new(),
                },
            ));
        }
        Ok(Flow::Continue)
    });
    adapter.try_stream(&r, &Demand::RICH, &mut sink).expect("stream");
    out
}

fn request(name: &str) -> Value {
    let text = std::fs::read_to_string(dir().join(format!("{name}.request.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn real_compactions_replay_as_they_matched() {
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(dir().join("expected.json")).unwrap()).unwrap();
    let cases = expected.as_object().unwrap();
    assert!(!cases.is_empty());
    for (name, want) in cases {
        let records = replay(&dir().join(format!("{name}.jsonl")));
        let input = request(name);
        let view = RequestView::parse(&input).unwrap();
        let (_, newest) = records.last().unwrap();
        let mut newest = newest.clone();
        newest.profile = view
            .instruction
            .as_ref()
            .map(|i| cv_core::precompact::profile_id(&i.text))
            .unwrap();

        let m = validate(&view, &newest).unwrap_or_else(|e| panic!("{name}: the newest cut missed: {e}"));
        assert_eq!(
            newest.anchors.len() as u64,
            want["anchors"].as_u64().unwrap(),
            "{name}: anchors"
        );
        assert_eq!(
            m.tail_from as u64,
            want["tail_from"].as_u64().unwrap(),
            "{name}: tail_from"
        );
        assert_eq!(
            view.tail(m.tail_from).len() as u64,
            want["tail_messages"].as_u64().unwrap(),
            "{name}: tail"
        );

        // An earlier summary: the last cut at or under 70% of the newest fill.
        let fill = records.iter().rev().find_map(|(f, _)| *f).unwrap();
        let (_, early) = records
            .iter()
            .rev()
            .find(|(f, _)| f.is_some_and(|f| f * 10 <= fill * 7))
            .unwrap_or_else(|| panic!("{name}: no cut at 70% of {fill}"));
        let mut early = early.clone();
        early.profile = newest.profile.clone();
        let e = validate(&view, &early).unwrap_or_else(|e| panic!("{name}: the early cut missed: {e}"));
        assert!(
            e.tail_from < m.tail_from || view.tail(e.tail_from).len() > view.tail(m.tail_from).len(),
            "{name}: the early cut's tail is no longer"
        );

        // The history under the summary changed: the last assistant text it covers, and the last
        // tool call (a seat whose model writes no prose between its calls has only those). The
        // request's head can hold calls from before the window, which no anchor covers.
        let mut edits = 0;
        for kind in ["text", "tool_use"] {
            let mut changed = input.clone();
            let messages = changed["body"]["messages"].as_array_mut().unwrap();
            let Some(target) = messages[..m.tail_from]
                .iter_mut()
                .rev()
                .filter(|m| m["role"] == "assistant")
                .flat_map(|m| m["content"].as_array_mut().into_iter().flatten())
                .find(|b| b["type"] == kind && (kind != "text" || b["text"].as_str().is_some_and(|t| t.len() >= 40)))
            else {
                continue;
            };
            if kind == "text" {
                target["text"] = Value::from("an answer that was never given, after a rewind of the session");
            } else {
                target["id"] = Value::from("toolu_never_called");
            }
            let view = RequestView::parse(&changed).unwrap();
            assert!(
                matches!(validate(&view, &newest), Err(Miss::Anchor { .. })),
                "{name}: a changed {kind} matched"
            );
            edits += 1;
        }
        assert!(edits > 0, "{name}: no assistant block to change");
    }
}
