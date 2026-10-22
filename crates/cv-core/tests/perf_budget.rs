//! The parse budget (docs/design/PERF.md), and the tools that measured it. All ignored by default.
//!
//! - `corpus_parse_holds_its_budget` is CI's gate: the counting pass `cv index` runs, over the
//!   committed corpus repeated to 64 MB, against a `std::fs::read` of the same bytes in the same run.
//!   `cargo test --release -p clustervision-core --test perf_budget -- --ignored corpus`
//! - `CV_PERF_IDS=<id>,<id> … -- --ignored --nocapture real_sessions` times the same pass on named
//!   real sessions (resolved through the catalog).
//! - `CV_PERF_DUMP=<file> … -- --ignored dump_stream_hashes` hashes every session's streams, so
//!   two builds can be shown to read a fleet identically.

use std::time::Instant;

use cv_core::{Adapter, Demand, Flow, Message, SessionRef, TryFnSink};

/// The pass a counts recount runs per session: a lazy stream, every message counted.
fn count_pass(adapter: &dyn Adapter, r: &SessionRef) -> (usize, cv_core::affordance::Counts) {
    let mut n = 0;
    let mut counter = cv_core::affordance::Counter::new(r);
    let mut sink = TryFnSink::new(|m: Message| {
        n += 1;
        counter.note(&m)?;
        Ok(Flow::Continue)
    });
    let s = adapter
        .try_stream(r, &Demand::LEAN.with_spans(), &mut sink)
        .expect("stream");
    (n, counter.finish(&s, &adapter.store_affordances(r).unwrap()))
}

fn best_of<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..runs {
        let t = Instant::now();
        let v = f();
        best = best.min(t.elapsed().as_secs_f64());
        out = Some(v);
    }
    (best, out.unwrap())
}

#[test]
#[ignore]
fn real_sessions() {
    let Ok(ids) = std::env::var("CV_PERF_IDS") else {
        eprintln!("set CV_PERF_IDS");
        return;
    };
    for id in ids.split(',') {
        let (r, adapter) = cv_core::find(id, None).expect("find").expect("no such session");
        let bytes = std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0) as f64;
        let (read_s, _) = best_of(3, || std::fs::read(&r.path).map(|v| v.len()).unwrap_or(0));
        let (parse_s, (n, counts)) = best_of(3, || count_pass(adapter.as_ref(), &r));
        println!(
            "{id:.12} {:>6} {:>8.1} MB  read {:>6.3} s  count {:>6.3} s  {:>6.0} MB/s  x{:.1} read  {n} msgs  {} kinds",
            r.harness.as_str(),
            bytes / 1e6,
            read_s,
            parse_s,
            bytes / 1e6 / parse_s,
            parse_s / read_s,
            counts.len()
        );
    }
}

/// `CV_PERF_DUMP=<file>`: for every cataloged session of the harnesses in `CV_PERF_HARNESS`
/// (default `claude,codex`), one line with a hash of every message a stream
/// produce (lazy, bulk and full), serialized — two builds of cv that write the same file read the fleet identically.
#[test]
#[ignore]
fn dump_stream_hashes() {
    use std::hash::{Hash, Hasher};
    use std::io::Write;
    let Ok(out) = std::env::var("CV_PERF_DUMP") else {
        eprintln!("set CV_PERF_DUMP");
        return;
    };
    let want = std::env::var("CV_PERF_HARNESS").unwrap_or_else(|_| "claude,codex".into());
    let want: Vec<&str> = want.split(',').collect();
    let mut refs: Vec<SessionRef> = cv_core::sessions()
        .into_iter()
        .filter(|r| want.contains(&r.harness.as_str()))
        .collect();
    refs.sort_by(|a, b| (a.harness.as_str(), &a.id, &a.path).cmp(&(b.harness.as_str(), &b.id, &b.path)));
    let lines: Vec<String> = {
        use std::sync::Mutex;
        let next = Mutex::new(0usize);
        let results = Mutex::new(vec![String::new(); refs.len()]);
        std::thread::scope(|s| {
            for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
                s.spawn(|| loop {
                    let i = {
                        let mut n = next.lock().unwrap();
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some(r) = refs.get(i) else { return };
                    let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
                        continue;
                    };
                    let mut line = format!("{} {} {}", r.harness.as_str(), r.id, r.path.display());
                    for opts in [Demand::LEAN.with_spans(), Demand::LEAN, Demand::RICH] {
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        let mut n = 0usize;
                        let result = {
                            let mut sink = |m: Message| {
                                n += 1;
                                serde_json::to_string(&m).unwrap().hash(&mut h);
                                Flow::Continue
                            };
                            adapter.stream(r, &opts, &mut sink)
                        };
                        match result {
                            Ok(mut s) => {
                                s.messages.clear();
                                serde_json::to_string(&s).unwrap().hash(&mut h);
                                line.push_str(&format!(" {n}:{:016x}", h.finish()));
                            }
                            Err(e) => line.push_str(&format!(" err:{e}")),
                        }
                    }
                    results.lock().unwrap()[i] = line;
                });
            }
        });
        results.into_inner().unwrap()
    };
    let mut f = std::fs::File::create(out).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

/// One harness's committed corpus transcripts, every line, repeated into a `target`-byte file laid
/// out where the harness's adapter discovers it under `root`; returns the adapter and the ref.
fn corpus_blob(harness: &str, root: &std::path::Path, target: usize) -> (Box<dyn Adapter>, SessionRef) {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(harness);
    let mut lines: Vec<String> = Vec::new();
    for e in walkdir::WalkDir::new(&corpus).sort_by_file_name() {
        let e = e.unwrap();
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") && !p.to_string_lossy().contains("/subagents/") {
            lines.extend(std::fs::read_to_string(p).unwrap().lines().map(str::to_string));
        }
    }
    assert!(!lines.is_empty(), "no {harness} corpus transcripts");
    let mut blob = String::new();
    while blob.len() < target {
        for l in &lines {
            blob.push_str(l);
            blob.push('\n');
        }
    }
    let path = match harness {
        "claude" => root.join(".claude/projects/-perf/0b1c2d3e-0000-4000-8000-000000000001.jsonl"),
        _ => root
            .join(".codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-0b1c2d3e-0000-4000-8000-000000000002.jsonl"),
    };
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, blob).unwrap();
    // After the write: an adapter keeps only the roots that exist when it is built.
    let adapter: Box<dyn Adapter> = match harness {
        "claude" => Box::new(cv_core::harness::claude::Claude::with_config_dir(&root.join(".claude"))),
        _ => Box::new(cv_core::harness::codex::Codex::with_home(&root.join(".codex"))),
    };
    let r = adapter
        .discover()
        .unwrap()
        .into_iter()
        .find(|r| r.path.file_name() == path.file_name())
        .expect("the adapter discovers the blob");
    (adapter, r)
}

/// The parse budget (docs/design/PERF.md), as a ratio so a slow CI runner moves both sides: the
/// counting pass over the committed corpus, repeated to 64 MB, may take at most this many times a
/// `std::fs::read` of the same bytes in the same run.
const BUDGET_RATIO: &[(&str, f64)] = &[("claude", CLAUDE_MAX_RATIO), ("codex", CODEX_MAX_RATIO)];
const CLAUDE_MAX_RATIO: f64 = 90.0;
const CODEX_MAX_RATIO: f64 = 90.0;

/// The CI gate: `cargo test --release -p clustervision-core --test perf_budget -- --ignored corpus`.
#[test]
#[ignore]
fn corpus_parse_holds_its_budget() {
    let root = std::env::temp_dir().join(format!("cv-perf-{}", std::process::id()));
    let mut failures = Vec::new();
    for &(harness, budget) in BUDGET_RATIO {
        let (adapter, r) = corpus_blob(harness, &root, 64 << 20);
        let bytes = std::fs::metadata(&r.path).unwrap().len() as f64;
        let (read_s, _) = best_of(7, || std::fs::read(&r.path).unwrap().len());
        let (parse_s, (n, _)) = best_of(7, || count_pass(adapter.as_ref(), &r));
        let ratio = parse_s / read_s;
        println!(
            "{harness:6} {:.0} MB  read {read_s:.4} s  count {parse_s:.4} s  {:.0} MB/s  x{ratio:.1} read (budget x{budget:.1})  {n} msgs",
            bytes / 1e6,
            bytes / 1e6 / parse_s
        );
        if ratio > budget {
            failures.push(format!("{harness}: x{ratio:.1} a plain read, budget x{budget:.1}"));
        }
    }
    std::fs::remove_dir_all(&root).ok();
    assert!(failures.is_empty(), "the parse is over its budget: {failures:?}");
}
