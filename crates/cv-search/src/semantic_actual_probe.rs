//! Offline, isolated evidence for the real adapters, cached model, refresh and publication path.
//!
//! Run `cargo test -p cv-search --release actual_model_refresh_probe -- --ignored --nocapture`
//! with a scratch `CLUSTERVISION_HOME`, a new `CV_SEMANTIC_PROBE_DIR` beneath it, and a copied
//! local model in `CV_SEARCH_MODEL` beneath it. The probe changes the copied model config.
use super::*;
use std::cell::Cell;
thread_local! { static CALLS: Cell<[usize; 3]> = const { Cell::new([0; 3]) }; }
fn record(index: usize) {
    CALLS.with(|calls| {
        let mut value = calls.get();
        value[index] += 1;
        calls.set(value);
    });
}
fn reset() {
    CALLS.with(|calls| calls.set([0; 3]));
}
fn calls() -> [usize; 3] {
    CALLS.with(Cell::get)
}

// Inject observations at the same operation boundaries used by production.
fn embed_sessions(path: &Path, refs: &[cv_core::SessionRef]) -> Result<usize> {
    embed_with(
        path,
        refs,
        |r| {
            record(0);
            crate::read_corpus_doc(r)
        },
        |spec| {
            record(1);
            load_model(spec)
        },
        |path, store| {
            record(2);
            save_store(path, store)
        },
    )
}
fn semantic_search(path: &Path, query: &str, k: usize) -> Result<Vec<Hit>> {
    ResidentSearch::new(path.to_path_buf()).search(
        query,
        k,
        crate::SearchFilter::default(),
        || resolve_model(false),
        |spec| {
            record(1);
            load_model(spec)
        },
        |model, query| model.encode_single(query),
    )
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
fn jsonl_files(root: &Path, paths: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            jsonl_files(&entry.path(), paths);
        } else if entry.path().extension().is_some_and(|s| s == "jsonl")
            && !entry.path().to_string_lossy().contains("/subagents/")
        {
            paths.push(entry.path());
        }
    }
}
fn reference(path: PathBuf, id: &str, harness: cv_core::Harness) -> cv_core::SessionRef {
    cv_core::SessionRef {
        id: id.into(),
        harness,
        path,
        cwd: Some(PathBuf::from("/frozen/project")),
        title: None,
        created_at: None,
        updated_at: None,
        message_count: Some(2),
    }
}
fn claude(path: &Path, content: &str, note: &str) {
    std::fs::write(path, format!("{}\n", serde_json::json!({"type":"user","uuid":"u","sessionId":"probe","note":note,"message":{"role":"user","content":content}}))).unwrap();
}
fn vectors(store: &Store) -> std::collections::BTreeMap<(String, String), Vec<u32>> {
    store
        .meta
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (m.session.harness.as_str().to_string(), m.session.id.clone()),
                store.vector(i).iter().map(|f| f.to_bits()).collect(),
            )
        })
        .collect()
}
fn refresh_stage(name: &str, path: &Path, refs: &[cv_core::SessionRef], expected: [usize; 3]) {
    reset();
    let start = std::time::Instant::now();
    assert_eq!(embed_sessions(path, refs).unwrap(), refs.len());
    let measured = calls();
    assert_eq!(measured, expected, "{name}");
    println!(
        "{}",
        serde_json::json!({"stage":name,"elapsed_us":start.elapsed().as_micros(),"adapter_reads":measured[0],"model_loads":measured[1],"saves":measured[2]})
    );
}

#[test]
#[ignore = "requires isolated scratch home and copied local CV_SEARCH_MODEL; never downloads"]
fn actual_model_refresh_probe() {
    let root = PathBuf::from(std::env::var_os("CV_SEMANTIC_PROBE_DIR").expect("scratch CV_SEMANTIC_PROBE_DIR"));
    let model = PathBuf::from(std::env::var_os("CV_SEARCH_MODEL").expect("copied local CV_SEARCH_MODEL"));
    let home = PathBuf::from(std::env::var_os("CLUSTERVISION_HOME").expect("isolated scratch home"));
    assert!(root.starts_with(&home) && model.starts_with(&home));
    assert_ne!(home, dirs::home_dir().unwrap().join(".clustervision"));
    assert!(model.is_dir());
    assert!(!root.exists(), "use a new scratch probe directory");
    std::fs::create_dir_all(&root).unwrap();
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cv-core/tests/corpus");
    let mut refs = Vec::new();
    for (name, version, harness, relative) in [
        ("claude", "2.1.296", cv_core::Harness::Claude, ".claude/projects"),
        ("codex", "0.162.1", cv_core::Harness::Codex, ".codex/sessions"),
        ("gemini", "0.63.0", cv_core::Harness::Gemini, ".gemini/tmp"),
    ] {
        let destination = root.join(name);
        copy_tree(&corpus.join(name).join(version), &destination);
        let mut files = Vec::new();
        jsonl_files(&destination.join(relative), &mut files);
        files.sort();
        assert!(files.len() >= 2);
        refs.extend(
            files
                .into_iter()
                .take(2)
                .enumerate()
                .map(|(i, path)| reference(path, &format!("{name}-{i}"), harness)),
        );
    }
    let args = root.join("arguments.jsonl");
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&args).unwrap());
        writeln!(file,"{}",serde_json::json!({"type":"user","uuid":"u","sessionId":"probe","message":{"role":"user","content":"Inspect large semantic arguments"}})).unwrap();
        file.write_all(br#"{"type":"assistant","uuid":"a","sessionId":"probe","message":{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Edit","input":{"content":""#).unwrap();
        let block = vec![b'x'; 65536];
        for _ in 0..512 {
            file.write_all(&block).unwrap();
        }
        file.write_all(b"\"}}]}}\n").unwrap();
    }
    refs.push(reference(args, "large-arguments", cv_core::Harness::Claude));
    let escaped = root.join("escaped.jsonl");
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&escaped).unwrap());
        file.write_all(br#"{"type":"user","uuid":"u","sessionId":"probe","message":{"role":"user","content":"Escaped semantic bridge "#).unwrap();
        let block = b"\\u0078".repeat(8192);
        for _ in 0..640 {
            file.write_all(&block).unwrap();
        }
        file.write_all(b"\"}}\n").unwrap();
    }
    refs.push(reference(escaped, "escaped-text", cv_core::Harness::Claude));
    for id in ["twin-a", "twin-b"] {
        let path = root.join(format!("{id}.jsonl"));
        claude(&path, "semantic shared twin astronomy", "initial");
        refs.push(reference(path, id, cv_core::Harness::Claude));
    }
    let source_bytes: u64 = refs.iter().map(|r| std::fs::metadata(&r.path).unwrap().len()).sum();
    let model_bytes: u64 = ["config.json", "tokenizer.json", "model.safetensors"]
        .into_iter()
        .map(|name| std::fs::metadata(model.join(name)).unwrap().len())
        .sum();
    println!(
        "{}",
        serde_json::json!({"stage":"fixture","sessions":refs.len(),"source_bytes":source_bytes,"model_bytes":model_bytes})
    );
    let store = root.join("vectors.bin");
    refresh_stage("initial", &store, &refs, [refs.len(), 1, 1]);
    let initial = load_store(&store).unwrap();
    assert!(initial.dim > 0);
    assert!(initial.vectors.iter().all(|v| v.is_finite()));
    assert!(initial
        .meta
        .iter()
        .find(|m| m.session.id == "large-arguments")
        .unwrap()
        .preview
        .starts_with("Inspect large semantic arguments Edit"));
    assert!(initial
        .meta
        .iter()
        .find(|m| m.session.id == "escaped-text")
        .unwrap()
        .preview
        .starts_with("Escaped semantic bridge"));
    let before = std::fs::read(&store).unwrap();
    let mtime = std::fs::metadata(&store).unwrap().modified().unwrap();
    refresh_stage("unchanged", &store, &refs, [0, 0, 0]);
    assert_eq!(std::fs::read(&store).unwrap(), before);
    assert_eq!(std::fs::metadata(&store).unwrap().modified().unwrap(), mtime);
    let changed = refs.iter().find(|r| r.id == "twin-a").unwrap();
    claude(
        &changed.path,
        "semantic shared twin astronomy",
        "changed source, same demanded body",
    );
    refresh_stage("one-source-changed", &store, &refs, [1, 1, 1]);
    let updated = load_store(&store).unwrap();
    assert_eq!(vectors(&initial), vectors(&updated));
    reset();
    let start = std::time::Instant::now();
    let hits = semantic_search(&store, "semantic shared twin astronomy", refs.len()).unwrap();
    assert_eq!(calls(), [0, 1, 0]);
    println!(
        "{}",
        serde_json::json!({"stage":"query","elapsed_us":start.elapsed().as_micros(),"adapter_reads":0,"model_loads":1,"saves":0,"top_ids":hits.iter().map(|h|&h.id).collect::<Vec<_>>()})
    );
    let clean = root.join("clean.bin");
    refresh_stage("clean-oracle", &clean, &refs, [refs.len(), 1, 1]);
    assert_eq!(vectors(&load_store(&clean).unwrap()), vectors(&updated));
    let clean_hits = semantic_search(&clean, "semantic shared twin astronomy", refs.len()).unwrap();
    let ranking_equal = hits.len() == clean_hits.len()
        && hits.iter().zip(&clean_hits).all(|(a, b)| {
            a.id == b.id
                && a.harness == b.harness
                && a.score.to_bits() == b.score.to_bits()
                && a.snippet == b.snippet
                && a.title == b.title
                && a.cwd == b.cwd
                && a.created_at == b.created_at
                && a.updated_at == b.updated_at
        });
    println!(
        "{}",
        serde_json::json!({"stage":"clean-ranking-oracle","equal":ranking_equal})
    );
    assert!(ranking_equal, "incremental and clean ranking differ");
    claude(
        &changed.path,
        "ocean marine coral reef biology",
        "changed demanded body",
    );
    refresh_stage("one-body-changed", &store, &refs, [1, 1, 1]);
    let changed_vectors = vectors(&load_store(&store).unwrap());
    let mut previous_vectors = vectors(&updated);
    assert_ne!(
        previous_vectors.remove(&("claude".into(), "twin-a".into())),
        changed_vectors.get(&("claude".into(), "twin-a".into())).cloned()
    );
    assert!(previous_vectors
        .iter()
        .all(|(id, v)| changed_vectors.get(id) == Some(v)));
    reset();
    let changed_hits = semantic_search(&store, "ocean marine coral reef biology", 1).unwrap();
    assert_eq!(changed_hits[0].id, "twin-a");
    assert!(changed_hits[0].snippet.contains("ocean marine coral reef biology"));
    assert_eq!(calls(), [0, 1, 0]);
    let config = model.join("config.json");
    let mut bytes = std::fs::read(&config).unwrap();
    bytes.push(b'\n');
    std::fs::write(&config, &bytes).unwrap();
    reset();
    assert!(semantic_search(&store, "astronomy", 1).is_err());
    assert_eq!(calls(), [0, 0, 0]);
    refresh_stage("model-artifact-changed", &store, &refs, [refs.len(), 1, 1]);
    assert_eq!(vectors(&load_store(&store).unwrap()), changed_vectors);
    // A store of the previous stored-format version (its magic byte) re-embeds whole.
    let mut obsolete = std::fs::read(&store).unwrap();
    obsolete[7] = obsolete[7].wrapping_sub(1);
    std::fs::write(&store, obsolete).unwrap();
    reset();
    assert!(semantic_search(&store, "astronomy", 1).is_err());
    assert_eq!(calls(), [0, 0, 0]);
    refresh_stage("format-version-changed", &store, &refs, [refs.len(), 1, 1]);
    refresh_stage("final-unchanged", &store, &refs, [0, 0, 0]);
    reset();
    let mut elapsed = Vec::new();
    for _ in 0..100 {
        let start = std::time::Instant::now();
        assert_eq!(embed_sessions(&store, &refs).unwrap(), refs.len());
        elapsed.push(start.elapsed().as_micros());
    }
    elapsed.sort_unstable();
    assert_eq!(calls(), [0, 0, 0]);
    println!(
        "{}",
        serde_json::json!({"stage":"unchanged-100","median_us":elapsed[50],"p95_us":elapsed[95],"adapter_reads":0,"model_loads":0,"saves":0})
    );
}
