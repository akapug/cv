use super::*;

/// A tiny but realistic Claude JSONL: an assistant tool_use, a user tool_result with a big payload
/// (both the content block and the toolUseResult mirror), then a recent turn that must be spared.
fn fixture(dir: &Path, big: &str) -> PathBuf {
    let sid = "11111111-1111-4111-8111-111111111111";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-06-16T00:00:00Z",
            "message":{"role":"user","content":"do the thing"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-06-16T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/big.log"}}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a1","timestamp":"2026-06-16T00:00:02Z",
            "toolUseResult":{"stdout":big},
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":big}]}}),
        // a recent assistant turn (within keep-last) — must be left verbatim
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a2","parentUuid":"u1","timestamp":"2026-06-16T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"/recent.log"}}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u2","parentUuid":"a2","timestamp":"2026-06-16T00:00:04Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":big}]}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    let body: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    std::fs::write(&path, body).unwrap();
    path
}

fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("cv-prune-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn assistant_usage(total: u64) -> Value {
    serde_json::json!({"type":"assistant",
        "message":{"usage":{"input_tokens":total,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
}

/// Parse fixture values into the shared one-pass representation the sizing helpers read.
fn pv(lines: &[Value]) -> Vec<Option<Value>> {
    lines.iter().cloned().map(Some).collect()
}

#[test]
fn window_cutoff_sizes_by_real_recorded_usage() {
    // 5 assistant turns, cumulative usage 100k..500k (one compaction segment) — 100k per turn.
    let lines: Vec<Value> = (1..=5).map(|k| assistant_usage(k * 100_000)).collect();
    let parsed = pv(&lines);
    // budget 250k → the LARGEST tail whose real size stays ≤ 250k is turns 3,4 (200k real).
    // (Keeping turn 2 as well would be 300k — over budget; the contract is ≤, never ≥.)
    assert_eq!(usage_window_cutoff(&parsed, 250_000), Some((3, 200_000, false)));
    // budget beyond the whole session → keep everything.
    assert_eq!(usage_window_cutoff(&parsed, 9_000_000), Some((0, 500_000, false)));
    // no usage records → None (caller falls back to a byte estimate).
    assert_eq!(
        usage_window_cutoff(&pv(&[serde_json::json!({"type":"user"})]), 100),
        None
    );
}

#[test]
fn window_cutoff_keeps_single_huge_turn_but_flags_overshoot() {
    let lines: Vec<Value> = (1..=5).map(|k| assistant_usage(k * 100_000)).collect();
    // Even the newest turn alone (100k) busts a 50k budget: keep that one turn, flag it — the
    // caller must warn that the ≤-budget contract could not be honored.
    assert_eq!(usage_window_cutoff(&pv(&lines), 50_000), Some((4, 100_000, true)));
}

#[test]
fn window_cutoff_skips_sidechain_usage() {
    // A sub-agent (isSidechain) turn carries a HUGE usage from its own context window. It must not
    // poison the main thread's sizing: with it ignored, the tail is sized purely by the main turns.
    let mut side = assistant_usage(950_000);
    side["isSidechain"] = Value::Bool(true);
    let lines = [assistant_usage(100_000), side, assistant_usage(200_000)];
    // Main turns: 0 (100k) and 1 (200k → delta 100k). Budget 150k → keep from main turn 1.
    assert_eq!(usage_window_cutoff(&pv(&lines), 150_000), Some((1, 100_000, false)));
}

#[test]
fn window_cutoff_reads_stashed_usage_after_revive() {
    // A revived session: usage was pinned to 50k, but the real count is in `_cv_orig_ctx`.
    let mk = |orig: u64| {
        serde_json::json!({"type":"assistant","_cv_orig_ctx":orig,
            "message":{"usage":{"input_tokens":50_000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
    };
    let lines = [mk(100_000), mk(200_000), mk(300_000)];
    // Sizing must use the stashed 100/200/300k (100k per turn), NOT the pinned 50k — else windowing
    // a chained (already-revived) session would size by garbage. Budget 150k fits one turn (100k).
    assert_eq!(usage_window_cutoff(&pv(&lines), 150_000), Some((2, 100_000, false)));
}

#[test]
fn prunes_old_payloads_into_new_session_keeping_recent() {
    let dir = tmpdir();
    let big = "X".repeat(5000); // > default min_size 2048
    let src = fixture(&dir, &big);

    let opts = PruneOptions {
        min_size: 2048,
        keep_last: 2,
        drop: false,
        thinking: false,
        new_id: None,
        copy_resources: false,
        revive: false,
        dry_run: false,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();

    // new session, distinct id + file
    assert_ne!(r.new_id, r.source_id);
    assert!(r.new_path.exists());
    assert!(r.new_size < r.original_size);
    assert!(r.pruned_count >= 1);
    assert!(r.est_context_tokens_saved > 0);

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // every line stamped with the NEW id, none with the old
    assert!(out.contains(&r.new_id));
    assert!(!out.contains(&r.source_id));
    // the OLD tool_result (toolu_1) is a marker; the big payload is gone from the line
    assert!(out.contains("[PRUNED id=toolu_1"));
    // the RECENT tool_result (toolu_2, within keep_last=2) is spared — its big payload survives
    // somewhere in the output (its tool_result line is left verbatim).
    assert!(out.contains(&big), "recent payload must be kept verbatim");
    let recent_result = out
        .lines()
        .find(|l| l.contains("tool_result") && l.contains("toolu_2"))
        .unwrap();
    assert!(recent_result.contains(&big), "recent tool_result kept");
    // old tool_result line no longer carries the raw payload
    let old_result = out
        .lines()
        .find(|l| l.contains("tool_result") && l.contains("toolu_1"))
        .unwrap();
    assert!(!old_result.contains(&big));

    // sidecar holds the original, retrievable byte-faithfully
    let sc = r.sidecar_path.expect("sidecar written");
    assert!(sc.exists());
    let restored = retrieve(&sc, "toolu_1").unwrap();
    assert_eq!(restored.as_str().unwrap(), big);
    // the toolUseResult mirror was also stashed
    assert!(retrieve(&sc, "toolu_1#tur").is_ok());

    // resulting session still parses as valid JSONL
    for line in out.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("valid json line");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_mode_writes_no_sidecar_and_never_advertises_retrieve() {
    let dir = tmpdir();
    let big = "Y".repeat(5000);
    let src = fixture(&dir, &big);
    // keep_last 0 → every turn is "old", so the drop removes ALL big payloads.
    let opts = PruneOptions {
        min_size: 2048,
        keep_last: 0,
        drop: true,
        thinking: false,
        new_id: Some("aaaa".into()),
        copy_resources: false,
        revive: false,
        dry_run: false,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();
    assert_eq!(r.new_id, "aaaa");
    assert!(r.sidecar_path.is_none());
    assert!(!dir.join("aaaa.flat.jsonl").exists());
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(out.contains("[PRUNED id=toolu_1"));
    assert!(!out.contains(&big)); // dropped entirely (nothing retained, no sidecar)
                                  // the payload is destroyed — the marker must say so, not point at a sidecar that doesn't exist
    assert!(
        out.contains("dropped (no sidecar)"),
        "drop markers say the payload is gone"
    );
    assert!(!out.contains("--retrieve"), "drop markers must not advertise retrieval");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_thinking_omits_old_reasoning_and_relinks_emptied_records() {
    let dir = tmpdir();
    let big = "R".repeat(5000);
    let sid = "22222222-2222-4222-8222-222222222222";
    // Old thinking-only record a0 (outside keep_last) and a recent one a2 (within it).
    let line = |uuid: &str, parent: Option<&str>, role: &str, content: serde_json::Value| {
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"parentUuid":parent,
            "timestamp":"2026-06-18T00:00:00Z","message":{"role":role,"content":content}})
    };
    let think = |t: &str| serde_json::json!([{"type":"thinking","thinking":t,"signature":"sig"}]);
    let lines = [
        line("u0", None, "user", serde_json::json!("start")),
        line("a0", Some("u0"), "assistant", think(&big)),
        line(
            "t0",
            Some("a0"),
            "assistant",
            serde_json::json!([{"type":"text","text":"done"}]),
        ),
        line("u1", Some("t0"), "user", serde_json::json!("more")),
        line("a1", Some("u1"), "assistant", serde_json::json!("ok")),
        line("u2", Some("a1"), "user", serde_json::json!("again")),
        line("a2", Some("u2"), "assistant", think(&big)),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    let input = lines.iter().map(|l| l.to_string() + "\n").collect::<String>();
    std::fs::write(&path, &input).unwrap();

    let opts = PruneOptions {
        min_size: 1024,
        keep_last: 2,
        thinking: true,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(out.len() < input.len(), "dropping thinking never grows the transcript");
    assert!(
        !out.contains("[PRUNED"),
        "no inline marker: it would give the model nothing"
    );
    // The emptied record is gone; its child points at its parent and the chain still resumes.
    assert!(!out.contains("\"uuid\":\"a0\""));
    let t0: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"t0\"")).unwrap()).unwrap();
    assert_eq!(t0["parentUuid"], "u0");
    crate::harness::claude::emit::verify_claude_resumable(&r.new_path, None).unwrap();
    // Recent thinking (a2, within keep_last=2) is kept verbatim.
    let a2 = out.lines().find(|l| l.contains("\"uuid\":\"a2\"")).unwrap();
    assert!(
        a2.contains(&big) && a2.contains("\"thinking\""),
        "recent thinking kept verbatim"
    );
    // The dropped block is retrievable by record uuid and block index.
    let restored = retrieve(r.sidecar_path.as_ref().unwrap(), "a0#think0").unwrap();
    assert_eq!(restored.get("thinking").and_then(|v| v.as_str()), Some(big.as_str()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn revive_rewrites_stale_usage_below_loaded_content() {
    let dir = tmpdir();
    let sid = "33333333-3333-4333-8333-333333333333";
    // A compaction boundary, then a small post-boundary window whose last assistant turn still
    // carries a giant *recorded* usage (the stale wall) even though the content is tiny.
    let line = |uuid: &str, role: &str, content: serde_json::Value, usage: Option<serde_json::Value>| {
        let mut m = serde_json::json!({"role":role,"content":content});
        if let Some(u) = usage {
            m["usage"] = u;
        }
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"timestamp":"2026-06-19T00:00:00Z","message":m})
    };
    let stale = serde_json::json!({"input_tokens":2,"output_tokens":10,
        "cache_read_input_tokens":975_000,"cache_creation_input_tokens":296,
        "cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":296}});
    let lines = [
        line("u0", "user", serde_json::json!("old turn"), None),
        serde_json::json!({"type":"system","subtype":"compact_boundary","sessionId":sid,"uuid":"b0",
            "timestamp":"2026-06-19T00:00:01Z"}),
        line("u1", "user", serde_json::json!("hi again"), None),
        line(
            "a1",
            "assistant",
            serde_json::json!([{"type":"text","text":"small reply"}]),
            Some(stale),
        ),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        revive: true,
        keep_last: 100, // don't snip anything — isolate the revive behavior
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();

    assert_eq!(
        r.usage_rewritten, 1,
        "the one inflated post-boundary usage record corrected"
    );
    assert_eq!(
        r.revive_old_tokens,
        Some(975_298),
        "reports the stale total it replaced"
    );
    let honest = r.revive_tokens.expect("honest figure written");
    assert!(
        honest < 25_000,
        "tiny post-boundary content → tiny honest figure (got {honest})"
    );

    // the written file carries the corrected usage, caches zeroed
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    let a1 = out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap();
    let v: serde_json::Value = serde_json::from_str(a1).unwrap();
    let u = v.pointer("/message/usage").unwrap();
    assert_eq!(u["cache_read_input_tokens"], 0);
    assert_eq!(u["cache_creation_input_tokens"], 0);
    assert_eq!(u["input_tokens"].as_u64().unwrap(), honest);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn usage_total_reads_the_last_request_iteration_like_claude_code() {
    // Claude Code ≥ 2.1.277 sizes context from `usage.iterations` (the last non-advisor/compaction
    // request) — the top-level counters are only a fallback. cv must see the same number.
    let u = |v: Value| usage_total(v.as_object().unwrap());
    let top =
        |t: u64| serde_json::json!({"input_tokens":t,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    // iterations present: the last request iteration wins over a top-level that says otherwise,
    // and a trailing advisor iteration is skipped
    let mut v = top(500_866);
    v["iterations"] = serde_json::json!([
        {"type":"message","input_tokens":2,"output_tokens":78,
         "cache_read_input_tokens":975_473,"cache_creation_input_tokens":1_031},
        {"type":"advisor_message","input_tokens":1,"output_tokens":1,
         "cache_read_input_tokens":9_999_999,"cache_creation_input_tokens":0},
    ]);
    assert_eq!(u(v), 976_506);
    // no iterations → top-level
    assert_eq!(u(top(123)), 123);
    // top-level zero → zero, whatever iterations say (Claude Code short-circuits there)
    let mut z = top(0);
    z["iterations"] = serde_json::json!([{"type":"message","input_tokens":5,"output_tokens":1,
        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}]);
    assert_eq!(u(z), 0);
    // a zero-context or malformed last iteration → top-level fallback
    let mut m = top(321);
    m["iterations"] = serde_json::json!([{"type":"message","input_tokens":0,"output_tokens":1,
        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}]);
    assert_eq!(u(m), 321);
    let mut m2 = top(321);
    m2["iterations"] = serde_json::json!([{"type":"message","input_tokens":7}]);
    assert_eq!(u(m2), 321);
}

#[test]
fn revive_pins_usage_iterations_too() {
    // The regression behind "a pruned session never resumes" on Claude Code 2.1.277+: revive pinned
    // the top-level usage but left `iterations[*]` at the stale wall figure — and that is what the
    // gate reads now, so it refused the (small) session client-side with a synthesized "Prompt is
    // too long" before sending anything.
    let dir = tmpdir();
    let sid = "44444444-4444-4444-8444-444444444444";
    let line = |uuid: &str, role: &str, content: serde_json::Value, usage: Option<serde_json::Value>| {
        let mut m = serde_json::json!({"role":role,"content":content});
        if let Some(u) = usage {
            m["usage"] = u;
        }
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z","message":m})
    };
    // Top level already small (as a pre-fix revive would have left it); the stale wall lives in the
    // request iteration; an advisor iteration rides along and must be left alone.
    let stale = serde_json::json!({"input_tokens":50_000,"output_tokens":10,
    "cache_read_input_tokens":0,"cache_creation_input_tokens":0,
    "cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},
    "iterations":[
        {"type":"message","input_tokens":2,"output_tokens":10,
         "cache_read_input_tokens":975_000,"cache_creation_input_tokens":296,
         "cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":296}},
        {"type":"advisor_message","input_tokens":1,"output_tokens":1,
         "cache_read_input_tokens":777,"cache_creation_input_tokens":0}
    ]});
    let lines = [
        line("u0", "user", serde_json::json!("old turn"), None),
        serde_json::json!({"type":"system","subtype":"compact_boundary","sessionId":sid,"uuid":"b0",
            "timestamp":"2026-09-19T00:00:01Z"}),
        line("u1", "user", serde_json::json!("hi again"), None),
        line(
            "a1",
            "assistant",
            serde_json::json!([{"type":"text","text":"small reply"}]),
            Some(stale),
        ),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        revive: true,
        keep_last: 100,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.usage_rewritten, 1);
    assert_eq!(
        r.revive_old_tokens,
        Some(975_298),
        "the stale figure is read from the request iteration, not the (already small) top level"
    );
    let honest = r.revive_tokens.expect("honest figure written");
    assert!(
        honest < 25_000,
        "tiny post-boundary content → tiny honest figure (got {honest})"
    );

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    let a1 = out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap();
    let v: serde_json::Value = serde_json::from_str(a1).unwrap();
    assert_eq!(
        v["_cv_orig_ctx"], 975_298,
        "the real (iteration) figure is stashed for a later --window"
    );
    let u = v.pointer("/message/usage").unwrap();
    assert_eq!(u["input_tokens"].as_u64().unwrap(), honest);
    assert_eq!(u["cache_read_input_tokens"], 0);
    let req = &u["iterations"][0];
    assert_eq!(
        req["input_tokens"].as_u64().unwrap(),
        honest,
        "request iteration pinned"
    );
    assert_eq!(req["cache_read_input_tokens"], 0);
    assert_eq!(req["cache_creation_input_tokens"], 0);
    assert_eq!(req["cache_creation"]["ephemeral_5m_input_tokens"], 0);
    assert_eq!(req["output_tokens"], 10, "output counts are real and untouched");
    let adv = &u["iterations"][1];
    assert_eq!(adv["cache_read_input_tokens"], 777, "advisor iteration left alone");
    // and what Claude Code will read back is the pinned figure
    assert_eq!(usage_total(u.as_object().unwrap()), honest);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn synthetic_notices_are_not_turns() {
    // Claude Code's client-side "Prompt is too long" rows are `assistant` records with
    // `model: "<synthetic>"`. They ride along as records but never count as turns.
    let dir = tmpdir();
    let sid = "55555555-5555-4555-8555-555555555555";
    let mk = |ty: &str, uuid: &str, content: Value| {
        serde_json::json!({"type":ty,"sessionId":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":ty,"content":content}})
    };
    let mut synth = mk(
        "assistant",
        "e1",
        serde_json::json!([{"type":"text","text":"Prompt is too long"}]),
    );
    synth["message"]["model"] = "<synthetic>".into();
    synth["isApiErrorMessage"] = true.into();
    let lines = [
        mk("user", "u0", "first".into()),
        synth.clone(),
        mk("assistant", "a1", serde_json::json!([{"type":"text","text":"reply"}])),
        mk("user", "u1", "second".into()),
        synth,
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    // turns are u0, a1, u1 — the notices don't count; `--range 1-` keeps a1 + u1 (and the notices,
    // which travel with the turns they precede)
    let opts = PruneOptions {
        keep_range: Some((1, None)),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.dropped_turns, 1);
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(!out.contains("\"uuid\":\"u0\""));
    assert!(out.contains("\"uuid\":\"a1\"") && out.contains("\"uuid\":\"u1\""));
    assert_eq!(
        out.matches("Prompt is too long").count(),
        2,
        "notices are kept as records, just not counted"
    );
    let turns = out
        .lines()
        .filter(|l| serde_json::from_str::<Value>(l).is_ok_and(|v| turn_kind(&v).is_some()))
        .count();
    assert_eq!(turns, 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn window_keeps_last_singleton_records_and_restamps_both_id_spellings() {
    let dir = tmpdir();
    let sid = "66666666-6666-4666-8666-666666666666";
    let turn = |ty: &str, uuid: &str, text: &str| {
        serde_json::json!({"type":ty,"sessionId":sid,"session_id":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":ty,"content":text}})
    };
    let lines = [
        serde_json::json!({"type":"custom-title","customTitle":"My rename","sessionId":sid}),
        serde_json::json!({"type":"agent-name","agentName":"Fixing prune","sessionId":sid}),
        serde_json::json!({"type":"ai-title","aiTitle":"old auto title","sessionId":sid}),
        turn("user", "u0", "first"),
        turn("assistant", "a0", "r0"),
        serde_json::json!({"type":"ai-title","aiTitle":"newer auto title","sessionId":sid}),
        turn("user", "u1", "second"),
        turn("assistant", "a1", "r1"),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let opts = PruneOptions {
        keep_range: Some((2, None)),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.dropped_turns, 2);
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(!out.contains("\"uuid\":\"u0\""));
    assert_eq!(
        out.matches("\"type\":\"custom-title\"").count(),
        1,
        "the /rename title survives a head drop"
    );
    assert_eq!(out.matches("\"type\":\"agent-name\"").count(), 1);
    assert_eq!(
        out.matches("\"type\":\"ai-title\"").count(),
        1,
        "only the LAST ai-title is kept"
    );
    assert!(out.contains("newer auto title") && !out.contains("old auto title"));
    // both id spellings restamped — the source id is gone entirely
    assert!(!out.contains(sid));
    assert_eq!(out.matches(&format!("\"session_id\":\"{}\"", r.new_id)).count(), 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn thinking_flatten_snips_signature_only_blocks() {
    // Fable-era transcripts keep `thinking: ""` + a ~600-byte signature per turn. On the wire those
    // blocks still cost hundreds of tokens each, so `--thinking` must not skip them for being small.
    let dir = tmpdir();
    let sid = "77777777-7777-4777-8777-777777777777";
    let sig = "C".repeat(600);
    let mk = |uuid: &str, ty: &str, content: Value| {
        serde_json::json!({"type":ty,"sessionId":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":ty,"content":content}})
    };
    let think = |reply: &str| serde_json::json!([{"type":"thinking","thinking":"","signature":sig},{"type":"text","text":reply}]);
    let lines = [
        mk("u0", "user", "hi".into()),
        mk("a0", "assistant", think("old reply")),
        mk("u1", "user", "again".into()),
        mk("a1", "assistant", think("recent reply")),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let opts = PruneOptions {
        thinking: true,
        keep_last: 2,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(
        r.pruned_count, 1,
        "the OLD signature-only block is snipped; the recent one (keep-last) stays"
    );
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    let a0: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"a0\"")).unwrap()).unwrap();
    assert_eq!(
        a0["message"]["content"],
        serde_json::json!([{"type":"text","text":"old reply"}])
    );
    let a1 = out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap();
    assert!(a1.contains(&sig));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rendered_attachments_count_toward_byte_estimates() {
    // A model-visible attachment (`rendered[]`) is prompt text; the honest figure must include it.
    let big = "x".repeat(35_000);
    let lines = [
        serde_json::json!({"type":"user","uuid":"u0","message":{"role":"user","content":"hi"}}),
        serde_json::json!({"type":"attachment","uuid":"t0","attachment":{"type":"instructions"},"rendered":[{"content":big}]}),
        serde_json::json!({"type":"attachment","uuid":"t1","attachment":{"type":"hook_success"}}),
        serde_json::json!({"type":"assistant","uuid":"a0","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
    ];
    let parsed = pv(&lines);
    let out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let honest = honest_window_tokens(
        &parsed,
        &out.iter().map(|l| l.contains(MARKER_PREFIX)).collect::<Vec<_>>(),
        0,
        true,
    );
    assert!(
        honest >= 10_000,
        "35k bytes of rendered reminder ≈ 10k tokens must be counted (got {honest})"
    );
    assert_eq!(line_est(&lines[2]).total(), 0, "an unrendered attachment costs nothing");
}

#[test]
fn revive_honors_recorded_delta_evidence_over_low_byte_estimate() {
    let dir = tmpdir();
    let sid = "44444444-4444-4444-8444-444444444444";
    // No boundary, nothing snipped in the second span: the recorded usage DELTA (30k between a0 and
    // a1) is hard evidence that span really costs 30k, even though its byte estimate is tiny (e.g.
    // dense code that tokenizes far above the byte estimate). Revive must never write a figure below that
    // evidence — the old byte-only estimator did, and let resumes through that blew the real limit.
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let big = "B".repeat(100_000);
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-06-20T00:00:00Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tx","content":big}]}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","timestamp":"2026-06-20T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"usage":usage(50_000)}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","timestamp":"2026-06-20T00:00:02Z",
            "message":{"role":"user","content":"tiny"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","timestamp":"2026-06-20T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"tiny reply"}],"usage":usage(80_000)}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    // keep_last 0 → the u0 tool_result is snipped, so its span falls back to a byte estimate; the
    // untouched u1..a1 span keeps its recorded 30k delta as a floor.
    let opts = PruneOptions {
        revive: true,
        keep_last: 0,
        min_size: 2048,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert!(r.pruned_count >= 1, "the big tool_result was snipped");
    let honest = r.revive_tokens.expect("stale records rewritten");
    assert!(
        honest >= 30_000,
        "honest figure must respect the recorded 30k delta evidence, got {honest}"
    );
    assert!(
        honest < 80_000,
        "…but still shed the snipped payload's stale total, got {honest}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Fixture for the full `--window` behavior: a leading summary (title) record, two prompt/reply
/// pairs with recorded usage, and a trailing non-turn record.
fn window_fixture(dir: &Path) -> PathBuf {
    let sid = "55555555-5555-4555-8555-555555555555";
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let lines = [
        serde_json::json!({"type":"summary","summary":"my session title","leafUuid":"u0"}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","parentUuid":null,"timestamp":"2026-06-21T00:00:00Z",
            "message":{"role":"user","content":"old prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","parentUuid":"u0","timestamp":"2026-06-21T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"old reply"}],"usage":usage(100_000)}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a0","timestamp":"2026-06-21T00:00:02Z",
            "message":{"role":"user","content":"new prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u1","timestamp":"2026-06-21T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"new reply"}],"usage":usage(200_000)}}),
        serde_json::json!({"type":"file-history-snapshot","messageId":"m1","snapshot":{"files":{}}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    path
}

#[test]
fn window_prune_reroots_within_budget_and_keeps_bookend_records() {
    let dir = tmpdir();
    let src = window_fixture(&dir);
    // 120k budget: each pair costs 100k real, so only the newest pair fits (≤ contract).
    let opts = PruneOptions {
        window: Some(120_000),
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();

    assert_eq!(r.dropped_turns, 2, "old prompt+reply dropped");
    let real = r.window_real_tokens.expect("sized by recorded usage");
    assert!(real <= 120_000, "≤-budget contract: kept tail is {real}, budget 120000");
    assert_eq!(real, 100_000);
    assert!(r.warnings.is_empty(), "no overshoot → no warning");

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // dropped turns are gone
    assert!(!out.contains("old prompt") && !out.contains("old reply"));
    // the kept tail opens on the user prompt, re-rooted (parentUuid nulled)
    let u1 = out.lines().find(|l| l.contains("\"uuid\":\"u1\"")).unwrap();
    let v: Value = serde_json::from_str(u1).unwrap();
    assert!(v["parentUuid"].is_null(), "first kept turn re-rooted");
    // the summary (title) record survives the head drop; the trailing snapshot survives the tail
    assert!(out.contains("my session title"), "leading summary record kept");
    assert!(out.contains("file-history-snapshot"), "trailing non-turn record kept");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn window_overshoot_keeps_the_huge_turn_and_warns() {
    let dir = tmpdir();
    let sid = "66666666-6666-4666-8666-666666666666";
    // No usage records → byte-estimate path. The NEWEST turn alone dwarfs the budget.
    let huge = "H".repeat(70_000); // ≈ 20k estimated tokens
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","parentUuid":null,"timestamp":"2026-06-22T00:00:00Z",
            "message":{"role":"user","content":"small"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","parentUuid":"u0","timestamp":"2026-06-22T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a0","timestamp":"2026-06-22T00:00:02Z",
            "message":{"role":"user","content":huge}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        window: Some(10),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.dropped_turns, 2, "everything but the huge newest turn dropped");
    assert!(!r.warnings.is_empty(), "overshoot must be reported loudly");
    assert!(
        r.warnings[0].contains("EXCEEDS"),
        "warning names the busted budget: {:?}",
        r.warnings
    );
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(
        out.contains(&huge),
        "the one oversized turn is still kept — never an empty session"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn window_and_revive_ignore_sidechain_turns() {
    let dir = tmpdir();
    let sid = "77777777-7777-4777-8777-777777777777";
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","parentUuid":null,"timestamp":"2026-06-23T00:00:00Z",
            "message":{"role":"user","content":"old prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","parentUuid":"u0","timestamp":"2026-06-23T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"old reply"}],"usage":usage(100_000)}}),
        // a sub-agent turn: its 950k usage measures the SUB-AGENT's context, not ours
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"sc0","parentUuid":null,"isSidechain":true,
            "timestamp":"2026-06-23T00:00:02Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"subagent says hi"}],"usage":usage(950_000)}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a0","timestamp":"2026-06-23T00:00:03Z",
            "message":{"role":"user","content":"new prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u1","timestamp":"2026-06-23T00:00:04Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"new reply"}],"usage":usage(200_000)}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        window: Some(120_000),
        revive: true,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();

    // Sizing saw main-thread costs of 100k per pair — the sidechain's 950k didn't poison it.
    assert_eq!(r.window_real_tokens, Some(100_000));
    assert_eq!(r.dropped_turns, 2, "only MAIN turns counted as dropped");
    assert!(r.warnings.is_empty());

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // The sidechain line rides along in the kept region as content…
    let sc = out
        .lines()
        .find(|l| l.contains("\"uuid\":\"sc0\""))
        .expect("sidechain line kept");
    // …but revive never rewrites ITS usage (a different context's number) …
    let scv: Value = serde_json::from_str(sc).unwrap();
    assert_eq!(
        scv.pointer("/message/usage/input_tokens").and_then(Value::as_u64),
        Some(950_000)
    );
    // …and the main-thread stale record was pinned to the honest main figure.
    assert_eq!(r.revive_tokens, Some(100_000));
    let a1: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap()).unwrap();
    assert_eq!(
        a1.pointer("/message/usage/input_tokens").and_then(Value::as_u64),
        Some(100_000)
    );
    // The re-root must be the first MAIN turn (u1), never the sidechain line.
    let u1: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"u1\"")).unwrap()).unwrap();
    assert!(u1["parentUuid"].is_null());
    assert!(
        scv["parentUuid"].is_null(),
        "sidechain parent untouched (was already null)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sidecar_ids_are_unique_and_round_trip_arrays_and_objects() {
    let dir = tmpdir();
    let sid = "88888888-8888-4888-8888-888888888888";
    let big_a = "A".repeat(4000);
    let big_b = "B".repeat(4000);
    // Two lines whose tool_result blocks carry NO tool_use_id (the collision-prone shape), each
    // with an array content payload and an object toolUseResult mirror.
    let arr = |t: &str| {
        serde_json::json!([
            {"type":"text","text":t},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
        ])
    };
    let mk = |uuid: &str, t: &str| {
        serde_json::json!({"type":"user","sessionId":sid,"uuid":uuid,"timestamp":"2026-06-24T00:00:00Z",
            "toolUseResult":{"stdout":t,"exit":0},
            "message":{"role":"user","content":[{"type":"tool_result","content":arr(t)}]}})
    };
    let lines = [mk("uA", &big_a), mk("uB", &big_b)];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        keep_last: 0,
        min_size: 1024,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.pruned_count, 4, "two content payloads + two mirrors");
    let sc = r.sidecar_path.expect("sidecar written");

    // Every sidecar id is unique — no line ever shadows another's payload.
    let raw = std::fs::read_to_string(&sc).unwrap();
    let ids: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut deduped = ids.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), ids.len(), "sidecar ids must be unique, got {ids:?}");

    // Array payloads (text + image blocks) round-trip verbatim, per line.
    assert_eq!(retrieve(&sc, "uA#tr0").unwrap(), arr(&big_a));
    assert_eq!(retrieve(&sc, "uB#tr0").unwrap(), arr(&big_b));
    // Object toolUseResult mirrors round-trip verbatim too.
    assert_eq!(
        retrieve(&sc, "uA#tr0#tur").unwrap(),
        serde_json::json!({"stdout":big_a,"exit":0})
    );
    assert_eq!(
        retrieve(&sc, "uB#tr0#tur").unwrap(),
        serde_json::json!({"stdout":big_b,"exit":0})
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn retrieve_errors_on_duplicate_ids_instead_of_guessing() {
    let dir = tmpdir();
    let sc = dir.join("dup.flat.jsonl");
    let entry = |content: &str| {
        serde_json::json!({"id":"dup","slot":"content","name":"Read","input":null,
            "content":content,"size":content.len(),"line_count":1,"kind":"text"})
        .to_string()
    };
    std::fs::write(
        &sc,
        format!("{}\n{}\n", entry("first payload"), entry("second payload")),
    )
    .unwrap();
    let err = retrieve(&sc, "dup").unwrap_err().to_string();
    assert!(err.contains("2 times"), "duplicate id must be an error, got: {err}");
    // a non-duplicated id in the same file still errors as not-found (with the listing)
    assert!(retrieve(&sc, "absent").is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn double_prune_is_idempotent() {
    let dir = tmpdir();
    let big = "D".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 0,
        ..Default::default()
    };
    let r1 = prune_session(&src, &opts).unwrap();
    let out1 = std::fs::read_to_string(&r1.new_path).unwrap();
    let markers1 = out1.matches(MARKER_PREFIX).count();
    assert!(markers1 > 0);

    // Pruning the pruned session again must find nothing new: markers are never re-snipped.
    let r2 = prune_session(&r1.new_path, &opts).unwrap();
    assert_eq!(r2.pruned_count, 0, "second prune snips nothing");
    assert!(r2.sidecar_path.is_none(), "no new sidecar for a no-op prune");
    let out2 = std::fs::read_to_string(&r2.new_path).unwrap();
    assert_eq!(out2.matches(MARKER_PREFIX).count(), markers1, "no nested markers");
    // the originals are still retrievable from the FIRST sidecar
    let restored = retrieve(r1.sidecar_path.as_ref().unwrap(), "toolu_1").unwrap();
    assert_eq!(restored.as_str().unwrap(), big);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pruned_session_reparses_via_claude_adapter_with_intact_turns() {
    // The in-repo resume-safety proxy: the pruned file must still parse through the Claude harness
    // adapter with the same turn structure (ids, roles, count) as the source.
    let dir = tmpdir();
    let big = "P".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 0,
        revive: true,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();

    let orig = crate::harness::claude::parse_str("orig", &std::fs::read_to_string(&src).unwrap(), None);
    let pruned_text = std::fs::read_to_string(&r.new_path).unwrap();
    let pruned = crate::harness::claude::parse_str("pruned", &pruned_text, None);

    assert!(!pruned.messages.is_empty());
    assert_eq!(pruned.messages.len(), orig.messages.len(), "same message count");
    for (o, p) in orig.messages.iter().zip(&pruned.messages) {
        assert_eq!(o.role, p.role, "roles preserved in order");
        assert_eq!(o.id, p.id, "message uuids preserved");
    }
    // and a windowed prune re-parses cleanly too, opening on a user turn
    let opts = PruneOptions {
        window: Some(1), // tiny budget → keeps only the newest turn (with an overshoot warning)
        ..Default::default()
    };
    let src2 = window_fixture(&dir);
    let r2 = prune_session(&src2, &opts).unwrap();
    let tail = crate::harness::claude::parse_str("tail", &std::fs::read_to_string(&r2.new_path).unwrap(), None);
    assert!(!tail.messages.is_empty(), "windowed tail still parses into turns");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dry_run_writes_nothing() {
    let dir = tmpdir();
    let big = "Z".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        dry_run: true,
        keep_last: 2,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();
    assert!(r.pruned_count >= 1);
    assert!(!r.new_path.exists(), "dry run must not write the new session");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn keep_last_large_spares_everything() {
    let dir = tmpdir();
    let big = "Q".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 100,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();
    assert_eq!(r.pruned_count, 0, "nothing is old enough to snip");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn declassify_snips_all_security_prose_keeps_benign() {
    // A safeguard classifier scores the WHOLE loaded context, so security-dense PROSE anywhere — old
    // OR recent — can downgrade a resumed seat. --declassify snips EVERY security-dense message (user
    // string + assistant text blocks) into the sidecar (recency does NOT protect it, unlike the
    // tool/thinking passes), while leaving benign turns verbatim.
    let dir = tmpdir();
    let sid = "22222222-2222-4222-8222-222222222222";
    let secret_user = "We have a cross-tenant auth bypass exploit; the credential exfil risk is high.";
    let secret_asst = "The vulnerability lets an attacker exfil credentials via the exploit chain.";
    let benign_user = "Great, unrelated. Remember: my favorite color is teal.";
    let recent_secret = "Recent turn: the security exploit vulnerability detail is also cleared.";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-07-01T00:00:00Z",
            "message":{"role":"user","content":secret_user}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-07-01T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":secret_asst}]}}),
        // benign turn -> keep verbatim (0 security terms)
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a1","timestamp":"2026-07-01T00:00:02Z",
            "message":{"role":"user","content":benign_user}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a2","parentUuid":"u1","timestamp":"2026-07-01T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"Noted, teal."}]}}),
        // RECENT but security-dense -> ALSO snipped (recency does not protect a trigger span)
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u2","parentUuid":"a2","timestamp":"2026-07-01T00:00:04Z",
            "message":{"role":"user","content":recent_secret}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    // keep_last default (25) would mark all turns "recent" — declassify must snip regardless.
    // Tokens are caller-supplied (cv ships no built-in list); supply the ones this session uses.
    let opts = PruneOptions {
        declassify: true,
        declassify_tokens: [
            "security",
            "exploit",
            "vulnerability",
            "cross-tenant",
            "auth bypass",
            "credential",
            "exfil",
            "attacker",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();

    // ALL security-dense prose is snipped: originals gone, markers present.
    assert!(!out.contains(secret_user), "security user prose must be snipped");
    assert!(!out.contains(secret_asst), "security assistant prose must be snipped");
    assert!(
        !out.contains(recent_secret),
        "RECENT security prose is also snipped (recency is no shield)"
    );
    assert!(out.contains("tool=declassified"), "a declassify marker must be present");
    assert!(r.pruned_count >= 3, "all three dense messages snipped");
    // benign turn is untouched.
    assert!(out.contains("my favorite color is teal"), "benign prose kept verbatim");
    // lossless: the snipped user text is retrievable from the sidecar.
    let sidecar = r.sidecar_path.clone().expect("sidecar written");
    let got = retrieve(&sidecar, "u0#declass").unwrap();
    assert_eq!(got.as_str(), Some(secret_user), "snipped prose retrievable verbatim");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn declassify_spares_benign_sessions_entirely() {
    // A session with < min_hits security terms per message must be left byte-untouched by declassify.
    let dir = tmpdir();
    let sid = "33333333-3333-4333-8333-333333333333";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-07-01T00:00:00Z",
            "message":{"role":"user","content":"Let's talk about the security fix we shipped."}}), // 1 term only
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-07-01T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"Sounds good, all resolved."}]}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let opts = PruneOptions {
        keep_last: 0,
        declassify: true,
        declassify_tokens: ["security", "exploit", "vulnerability", "credential", "exfil"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert_eq!(
        r.pruned_count, 0,
        "a single security term (< min_hits=2) must not trip the snip"
    );
    assert!(
        out.contains("the security fix we shipped"),
        "benign-density prose kept verbatim"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn decision_roster_preserves_selection_and_revive_evidence() {
    let rows = vec![
        serde_json::json!({"type":"user","message":{"content":"opening prompt"}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"reasoning","signature":"sig"}],"usage":{"input_tokens":12,"cache_read_input_tokens":5,"iterations":[{"type":"message","input_tokens":30,"cache_read_input_tokens":4,"cache_creation_input_tokens":1,"output_tokens":2}]}}}),
        serde_json::json!({"type":"assistant","isSidechain":true,"message":{"content":"other context","usage":{"input_tokens":999999}}}),
        serde_json::json!({"type":"system","subtype":"compact_boundary"}),
        serde_json::json!({"type":"attachment","attachment":{"type":"memory","rendered":["system reminder"]}}),
        serde_json::json!({"type":"assistant","message":{"model":"<synthetic>","content":"Prompt too long"}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"[PRUNED id=t]"}]}}),
        serde_json::json!({"type":"assistant","_cv_orig_ctx":120,"message":{"content":"answer","usage":{"input_tokens":100}}}),
    ];
    let original = pv(&rows);
    let facts: Vec<_> = rows.iter().map(|v| Some(decision_facts(v, true))).collect();
    for budget in [0, 1, 30, 100, 200, 1000] {
        let options = PruneOptions {
            window: Some(budget),
            ..Default::default()
        };
        assert_eq!(
            select_kept_turns(&original, &options, 4),
            select_kept_turns(&facts, &options, 4)
        );
    }
    let pruned: Vec<_> = rows.iter().map(|v| v.to_string().contains(MARKER_PREFIX)).collect();
    for window in [0, 4] {
        for trusted in [true, false] {
            assert_eq!(
                honest_window_tokens(&original, &pruned, window, trusted),
                honest_window_tokens(&facts, &pruned, window, trusted)
            );
        }
    }
}

#[test]
fn sidecar_preparation_failure_preserves_existing_transcript() {
    let dir = tmpdir();
    let src = fixture(&dir, &"payload".repeat(1024));
    let new_id = "22222222-2222-4222-8222-222222222222";
    let transcript = dir.join(format!("{new_id}.jsonl"));
    std::fs::write(&transcript, "prior destination").unwrap();
    std::fs::create_dir(dir.join(format!("{new_id}.flat.jsonl"))).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        force: true,
        ..Default::default()
    };
    assert!(prune_session(&src, &options).is_err());
    assert_eq!(std::fs::read_to_string(transcript).unwrap(), "prior destination");
    assert!(!std::fs::read_dir(&dir).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".cv-prune-")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn renamed_source_cannot_be_its_own_prune_destination() {
    let dir = tmpdir();
    let src = fixture(&dir, &"payload".repeat(1024));
    let new_id = "22222222-2222-4222-8222-222222222222";
    let renamed = dir.join(format!("{new_id}.jsonl"));
    std::fs::rename(src, &renamed).unwrap();
    let original = std::fs::read(&renamed).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        ..Default::default()
    };
    assert!(prune_session(&renamed, &options).is_err());
    assert_eq!(std::fs::read(renamed).unwrap(), original);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn projected_source_facts_match_owned_metadata_and_token_estimates() {
    let dir = tmpdir();
    let path = dir.join("projection.jsonl");
    for row in [
        serde_json::json!({"type":"user","message":{"content":"escaped\n☃ prompt"},"ignored":"irrelevant"}),
        serde_json::json!({"type":"assistant","isSidechain":true,"message":{"model":"<synthetic>","usage":{"input_tokens":100,"iterations":[{"type":"message","input_tokens":40,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"output_tokens":1}]},"content":[{"type":"text","text":"answer ☃"},{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/a"}},{"type":"image"}]}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":{"number":1.5,"parts":["a",null,true]}},"literal",{"type":"text","text":null}]}}),
        serde_json::json!({"type":"attachment","rendered":[{"content":"system reminder"},{"content":1}]}),
    ] {
        let line = row.to_string();
        std::fs::write(&path, &line).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let options = PruneOptions {
            revive: true,
            ..Default::default()
        };
        let mut tools = HashMap::new();
        let range = Range {
            offset: 0,
            len: line.len() as u64,
        };
        let mut facts = source
            .read_range_value(0, range.len, |v| decision_facts_source(v, range, &options, &mut tools))
            .unwrap();
        for key in ["_prune_payload", "_prune_thinking", "_prune_parent"] {
            facts.as_object_mut().unwrap().remove(key);
        }
        assert_eq!(facts, decision_facts(&row, true));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn giant_ignored_row_preserves_raw_bytes_and_recorded_usage() {
    let dir = tmpdir();
    let sid = "11111111-1111-4111-8111-111111111111";
    let new_id = "22222222-2222-4222-8222-222222222222";
    let line = format!(" {{ \"sessionId\":\"{sid}\", \"type\":\"user\", \"message\":{{\"content\":\"prompt\"}}, \"unrelated\":\"{}[PRUNED id=fake]{}\" }}", "x".repeat(LARGE_RECORD as usize), "x".repeat(LARGE_RECORD as usize));
    let usage = serde_json::json!({"type":"assistant","sessionId":sid,"message":{"content":"answer","usage":{"input_tokens":100000000}}});
    let src = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&src, format!("{line}\n{usage}\n")).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        revive: true,
        keep_last: 0,
        ..Default::default()
    };
    let result = prune_session(&src, &options).unwrap();
    let output = std::fs::read_to_string(result.new_path).unwrap();
    assert_eq!(
        output.lines().next().unwrap(),
        crate::restamp_line(&line, new_id).unwrap()
    );
    assert_eq!(result.pruned_count, 0);
    // A marker-shaped string outside the model-visible content is no evidence: nothing was pruned,
    // so the recorded prompt stands and nothing is rewritten.
    assert_eq!(result.usage_rewritten, 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn marker_writer_detects_every_chunk_split_with_bounded_tail() {
    let bytes = format!("prefix {MARKER_PREFIX}fake] suffix").into_bytes();
    for split in 0..=bytes.len() {
        let mut output = Vec::new();
        let mut writer = MarkerWriter::new(&mut output);
        writer.write_all(&bytes[..split]).unwrap();
        writer.write_all(&bytes[split..]).unwrap();
        assert!(writer.found);
        assert!(writer.tail.len() < MARKER_PREFIX.len());
        assert_eq!(output, bytes);
    }
}

/// An existing destination, a symlink or hard link included, is refused without a byte written;
/// `force` replaces the name by rename, leaving the old inode (and what it links to) intact.
#[cfg(unix)]
#[test]
fn existing_output_is_refused_and_force_replaces_the_name_not_the_inode() {
    use std::os::unix::fs::{symlink, MetadataExt};
    for symbolic in [true, false] {
        let dir = tmpdir();
        let source = fixture(&dir, &"payload".repeat(1024));
        let new_id = "22222222-2222-4222-8222-222222222222";
        let target = dir.join("existing-target");
        let destination = dir.join(format!("{new_id}.jsonl"));
        std::fs::write(&target, "before").unwrap();
        if symbolic {
            symlink(&target, &destination).unwrap();
        } else {
            std::fs::hard_link(&target, &destination).unwrap();
        }
        let inode = std::fs::metadata(&target).unwrap().ino();
        let mut options = PruneOptions {
            new_id: Some(new_id.into()),
            keep_last: 0,
            ..Default::default()
        };
        let error = prune_session(&source, &options).unwrap_err();
        assert!(error.downcast_ref::<crate::publish::Exists>().is_some(), "{error:#}");
        assert!(!dir.join(format!("{new_id}.flat.jsonl")).exists());
        options.force = true;
        prune_session(&source, &options).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), inode);
        assert!(!std::fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&destination).unwrap().contains(MARKER_PREFIX));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn hardlinked_source_is_rejected_before_truncation() {
    let dir = tmpdir();
    let source = fixture(&dir, &"payload".repeat(1024));
    let original = std::fs::read(&source).unwrap();
    let new_id = "22222222-2222-4222-8222-222222222222";
    std::fs::hard_link(&source, dir.join(format!("{new_id}.jsonl"))).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        ..Default::default()
    };
    assert!(prune_session(&source, &options).is_err());
    assert_eq!(std::fs::read(source).unwrap(), original);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn source_string_mutation_matches_owned_output_sidecars_and_estimate() {
    for drop in [false, true] {
        let world = cv_testkit::World::new("prune-source-string-mutation");
        let text = format!("{}\n{}\r\n🌙\\\"\n", "x".repeat(1024 * 1024), "a".repeat(70000));
        let mut owned = serde_json::json!({"type":"user","sessionId":"old","session_id":"old2","uuid":"line","message":{"content":[
            {"type":"tool_result","tool_use_id":"t","content":text,"extra":1.50},
            {"type":"text","text":"keep"},
            {"type":"tool_result","content":"small"},
            {"type":"tool_result","content":"two\nlines\n"},
            {"type":"image","source":{"data":"small"}}
        ]},"parentUuid":"p"});
        let raw = serde_json::to_string_pretty(&owned).unwrap();
        let raw = format!("{{\"sessionId\":\"shadow\",{}", raw.strip_prefix('{').unwrap());
        owned = serde_json::from_str(&raw).unwrap();
        let path = world.base.join("input.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let range = Range {
            offset: 0,
            len: raw.len() as u64,
        };
        source.read_range_value(0, range.len, |_| Ok(())).unwrap();
        let opts = PruneOptions {
            min_size: 4,
            drop,
            revive: true,
            ..Default::default()
        };
        let plan = source_user_prune(&source, range, &opts, "new", 0, &HashMap::new())
            .unwrap()
            .unwrap();
        let mut sidecar = Vec::new();
        let mut count = 0;
        let mut images = 0;
        let mut saved = 0;
        let mut modified = false;
        prune_user_line(
            &mut owned,
            &opts,
            &HashMap::new(),
            "new",
            "line",
            &mut sidecar,
            &mut count,
            &mut images,
            &mut saved,
            &mut modified,
        );
        let expected = crate::restamp_line(&owned.to_string(), "new").unwrap();
        let mut actual = Vec::new();
        plan.record
            .with_view(|v| v.write_canonical_replacing(&plan.replacements, &mut actual))
            .unwrap();
        assert_eq!(actual, expected.as_bytes());
        assert_eq!(plan.saved, saved);
        assert_eq!(plan.payloads.len(), count);
        assert_eq!(plan.estimate, line_est(&owned));
        if !drop {
            let mut expected = Vec::new();
            for entry in &sidecar {
                serde_json::to_writer(&mut expected, entry).unwrap();
                expected.push(b'\n');
            }
            let mut actual = Vec::new();
            for payload in &plan.payloads {
                write_source_payload(payload, &mut actual).unwrap();
            }
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn source_user_mutation_matches_rich_payload_and_mirror_owned_oracle() {
    let cases = [
        serde_json::json!({"message":{"content":[{"type":"tool_result","content":[
            {"type":"text","text":"first\n"},{"type":"text","text":""},
            {"type":"image","source":{"data":"x".repeat(1024*1024)}},
            {"type":"text","text":"tail"},{"type":"image"}
        ]}]},"toolUseResult":{"file":{"base64":null},"text":"mirror"}}),
        serde_json::json!({"message":{"content":[{"type":"tool_result","tool_use_id":"absent"},
            {"type":"tool_result","content":[]},{"type":"tool_result","content":{}},
            {"type":"tool_result","content":[{"unknown":"payload"}]}]},
            "toolUseResult":{"type":"image","body":"mirror"}}),
        serde_json::json!({"message":{"content":"prompt\n"},"toolUseResult":"mirror\nlines\n"}),
        serde_json::json!({"message":{"content":[{"type":"tool_result","content":"[PRUNED id=already]"}]},
            "toolUseResult":"[PRUNED id=mirror]"}),
    ];
    for original in cases {
        for drop in [false, true] {
            let world = cv_testkit::World::new("source-rich-user-prune");
            let mut owned = original.clone();
            owned["type"] = Value::String("user".into());
            owned["sessionId"] = Value::String("old".into());
            let raw = serde_json::to_string_pretty(&owned).unwrap();
            let path = world.base.join("input.jsonl");
            std::fs::write(&path, &raw).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let range = Range {
                offset: 0,
                len: raw.len() as u64,
            };
            source.read_range_value(0, range.len, |_| Ok(())).unwrap();
            let opts = PruneOptions {
                min_size: 4,
                drop,
                revive: true,
                ..Default::default()
            };
            let plan = source_user_prune(&source, range, &opts, "new", 0, &HashMap::new()).unwrap();
            let mut entries = Vec::new();
            let mut count = 0;
            let mut images = 0;
            let mut saved = 0;
            let mut modified = false;
            prune_user_line(
                &mut owned,
                &opts,
                &HashMap::new(),
                "new",
                "L0",
                &mut entries,
                &mut count,
                &mut images,
                &mut saved,
                &mut modified,
            );
            let Some(plan) = plan else {
                assert!(!modified);
                continue;
            };
            assert!(modified);
            assert_eq!(plan.images, images);
            assert_eq!(plan.payloads.len(), count);
            assert_eq!(plan.saved, saved);
            assert_eq!(plan.estimate, line_est(&owned));
            let mut actual = Vec::new();
            plan.record
                .with_view(|v| v.write_canonical_replacing(&plan.replacements, &mut actual))
                .unwrap();
            assert_eq!(
                actual,
                crate::restamp_line(&owned.to_string(), "new").unwrap().as_bytes()
            );
            if !drop {
                let mut expected = Vec::new();
                for entry in entries {
                    serde_json::to_writer(&mut expected, &entry).unwrap();
                    expected.push(b'\n');
                }
                let mut actual = Vec::new();
                for payload in plan.payloads {
                    write_source_payload(&payload, &mut actual).unwrap();
                }
                assert_eq!(actual, expected);
            }
        }
    }
}

#[test]
fn source_reroot_preserves_owned_stamp_order_including_duplicate_ids() {
    for ids in [
        r#""sessionId":"old","session_id":"other","#,
        r#""sessionId":"shadow","sessionId":"old","#,
    ] {
        let world = cv_testkit::World::new("source-prune-reroot");
        let raw=format!("{{{ids}\"type\":\"user\",\"parentUuid\":\"parent\",\"ignored\":\"{}\",\"message\":{{\"content\":\"[PRUNED id=existing]\"}}}}","x".repeat(1024*1024));
        let path = world.base.join("input.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let spans = source
            .read_range_value(0, raw.len() as u64, crate::restamp::source_id_spans)
            .unwrap();
        let record = source.validated_range(0, raw.len() as u64).unwrap();
        let expected = crate::restamp_line(&raw, "new").unwrap_or(raw);
        let mut expected: Value = serde_json::from_str(&expected).unwrap();
        expected["parentUuid"] = Value::Null;
        let mut actual = Vec::new();
        assert!(write_source_reroot(&record, &source, "new", &spans, &mut actual).unwrap());
        assert_eq!(actual, format!("{expected}\n").as_bytes());
    }
}

fn retrieve_legacy_oracle(path: &Path, id: &str) -> Result<Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading sidecar {}", path.display()))?;
    let mut available = Vec::new();
    let mut found = None;
    let mut duplicates = 0;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<SidecarEntry>(line) else {
            continue;
        };
        available.push(entry.id.clone());
        if entry.id == id {
            if found.is_some() {
                duplicates += 1;
            } else {
                found = Some(entry.content);
            }
        }
    }
    if duplicates > 0 {
        anyhow::bail!(
            "id {id:?} appears {} times in sidecar {} — ambiguous, refusing to guess which payload you meant",
            duplicates + 1,
            path.display()
        );
    }
    found.ok_or_else(|| {
        anyhow::anyhow!(
            "id {id:?} not found in sidecar. available: {}",
            available.iter().take(20).cloned().collect::<Vec<_>>().join(", ")
        )
    })
}

#[test]
fn retrieve_projection_matches_typed_legacy_acceptance_and_error_semantics() {
    let world = crate::test_world("retrieve-projection-oracle");
    let path = world.base.join("session.flat.jsonl");
    let row = |input: &str, content: &str, tail: &str| {
        format!(
            r#"{{"id":"wanted","slot":"content","name":"Read","input":{input},"content":{content},"size":1,"line_count":1,"kind":"text"{tail}}}"#
        )
    };
    let valid = row("null", r#"{"actual":["雪",true,null]}"#, "");
    let mut cases = vec![
        valid.clone(),
        format!("\r\n\u{2003}\nnot JSON\n{valid}\r\n"),
        format!("\u{2003}{valid}"),
        row("null", "null", ""),
        row("null", r#""actual""#, r#", "unknown":"\ud800", "number":1e400"#),
        row("null", r#""actual""#, r#", "unknown":null, "unknown":"\ud800"#),
        row("null", r#""actual""#, r#", "id":"wanted""#),
        row("null", r#""actual""#, r#", "\u0069d":"wanted""#),
        row("null", r#""actual""#, r#", "\ud800":null"#),
        row("1e400", r#""actual""#, ""),
        row("null", r#""\ud800""#, ""),
        row("null", "1e400", ""),
        valid.replace(r#""size":1"#, r#""size":1.0"#),
        valid.replace(r#""line_count":1"#, r#""line_count":-1"#),
        valid.replace(r#""size":1"#, r#""size":18446744073709551616"#),
        valid.replace(r#""size":1"#, r#""size":1e400"#),
        valid.replace(r#""name":"Read""#, r#""name":null"#),
        valid.replace(r#""input":null,""#, ""),
        r#"["wanted","content","Read",null,{"actual":"🌙"},1,1,"text"]"#.into(),
        r#"["wanted","content","Read",null,"actual",1,1,"text",null]"#.into(),
        r#"["wanted","content","Read",null,"actual",1,1]"#.into(),
        format!("{valid}\n{valid}\n{valid}"),
    ];
    for depth in 124..=130 {
        let nested = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        cases.push(row(&nested, "null", ""));
        cases.push(row("null", "null", &format!(",\"unknown\":{nested}")));
    }
    for integer in [
        "-0",
        "0.0",
        "1e0",
        "18446744073709551615",
        "18446744073709551616",
        "true",
        "null",
        "-1",
    ] {
        cases.push(valid.replace(r#""size":1"#, &format!("\"size\":{integer}")));
        cases.push(valid.replace(r#""line_count":1"#, &format!("\"line_count\":{integer}")));
    }
    for key in ["id", "slot", "name", "input", "content", "size", "line_count", "kind"] {
        let mut missing: Value = serde_json::from_str(&valid).unwrap();
        missing.as_object_mut().unwrap().remove(key);
        cases.push(missing.to_string());
    }
    for (i, case) in cases.into_iter().enumerate() {
        std::fs::write(&path, case).unwrap();
        for id in ["wanted", "absent"] {
            let expected = retrieve_legacy_oracle(&path, id);
            let actual = retrieve(&path, id);
            match (expected, actual) {
                (Ok(expected), Ok(actual)) => assert_eq!(actual, expected, "case {i}"),
                (Err(expected), Err(actual)) => assert_eq!(actual.to_string(), expected.to_string(), "case {i}"),
                (expected, actual) => panic!("case {i}: expected {expected:?}, actual {actual:?}"),
            }
        }
    }
    let mut bytes = valid.into_bytes();
    bytes.extend_from_slice(b"\nmalformed\xff");
    std::fs::write(&path, bytes).unwrap();
    let expected = retrieve_legacy_oracle(&path, "wanted").unwrap_err();
    let actual = retrieve(&path, "wanted").unwrap_err();
    assert_eq!(actual.to_string(), expected.to_string());
    assert_eq!(format!("{actual:#}"), format!("{expected:#}"));
}

#[test]
fn retrieve_bounds_available_ids_and_rejects_captured_source_changes() {
    let world = crate::test_world("retrieve-source-guard");
    let path = world.base.join("session.flat.jsonl");
    let mut out = std::fs::File::create(&path).unwrap();
    for i in 0..40 {
        writeln!(
            out,
            "{}",
            sidecar_line(&format!("id-{i}"), "tool", &Value::Null, "actual")
        )
        .unwrap();
    }
    drop(out);
    assert_eq!(
        retrieve(&path, "absent").unwrap_err().to_string(),
        retrieve_legacy_oracle(&path, "absent").unwrap_err().to_string()
    );
    let captured = crate::json::JsonSource::capture(&path).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    // Truncation, not growth: an append-only log may grow under a captured source.
    std::fs::write(&path, b"").unwrap();
    assert!(retrieve_captured(&captured, &file, &path, "id-0").is_err());
}

#[test]
fn retrieve_giant_unselected_fields_return_only_requested_content() {
    let world = crate::test_world("retrieve-giant-unselected");
    let path = world.base.join("session.flat.jsonl");
    let mut out = std::fs::File::create(&path).unwrap();
    let chunk = "ignored-🌙\\n\\\"\\\\".repeat(4096);
    out.write_all(br#"{"id":"other-"#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#"","slot":"content","name":""#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#"","input":{"body":""#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#""},"content":{"body":""#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#""},"size":1,"line_count":1,"kind":"text"}"#).unwrap();
    out.write_all(b"\n").unwrap();
    let chosen = serde_json::json!({"actual":["雪🌙",true,null]});
    let entry: SidecarEntry = SidecarEntry {
        id: "wanted".into(),
        slot: "content".into(),
        name: "tool".into(),
        input: Value::Null,
        content: chosen.clone(),
        size: 1,
        line_count: 1,
        kind: "text".into(),
    };
    serde_json::to_writer(&mut out, &entry).unwrap();
    drop(out);
    assert_eq!(retrieve(&path, "wanted").unwrap(), chosen);
    assert_eq!(
        retrieve(&path, "wanted").unwrap(),
        retrieve_legacy_oracle(&path, "wanted").unwrap()
    );
    assert!(retrieve(&path, "absent").unwrap_err().to_string().contains(", wanted"));
    assert_eq!(
        retrieve(&path, "absent").unwrap_err().to_string(),
        retrieve_legacy_oracle(&path, "absent").unwrap_err().to_string()
    );
}

#[test]
fn source_turn_projection_matches_owned_pass_composition() -> Result<()> {
    let world = crate::test_world("prune-turn-projection");
    for (case, mut original) in [
        serde_json::json!({"type":"assistant","uuid":"a","sessionId":"old","parentUuid":"p","message":{"content":[
            {"type":"thinking","thinking":"reason\r\nsecond λ\n","signature":"signature","unknown":{"keep":true}},
            {"type":"text","text":"ALPHA beta ΟΣ ΟΣΑ","extra":"retained in sidecar"},
            {"type":"thinking","signature":"signature-only"},
            {"type":"text","text":42},
            {"type":"text","text":"benign"}
        ]}}),
        serde_json::json!({"type":"user","uuid":"u","sessionId":"old","message":{"content":[
            {"type":"text","text":"ALPHA beta"},
            {"type":"tool_result","tool_use_id":"call","content":"payload"},
            {"type":"text","text":"benign"}
        ]},"toolUseResult":{"mirror":"payload"}}),
        serde_json::json!({"type":"user","uuid":"u","session_id":"old","message":{"content":"ALPHA beta\r\n"}}),
    ]
    .into_iter()
    .enumerate()
    {
        original["ignored"] = Value::String("not selected".repeat(100000));
        let raw = serde_json::to_string_pretty(&original)?.replace(
            "\"sessionId\": \"old\"",
            "\"sessionId\":\"first\", \"sessionId\":\"old\"",
        );
        let path = world.base.join(format!("{case}.json"));
        std::fs::write(&path, &raw)?;
        for drop in [false, true] {
            for thinking in [false, true] {
                for min_hits in [0, 2] {
                    let opts = PruneOptions {
                        min_size: 1,
                        drop,
                        thinking,
                        revive: true,
                        declassify: true,
                        declassify_min_hits: min_hits,
                        declassify_tokens: vec!["alpha".into(), "beta".into(), "ος".into()],
                        ..Default::default()
                    };
                    let mut owned = original.clone();
                    let mut sidecar = Vec::new();
                    let mut count = 0;
                    let mut images = 0;
                    let mut saved = 0;
                    let mut modified = false;
                    let user = owned["type"] == "user";
                    if user {
                        prune_user_line(
                            &mut owned,
                            &opts,
                            &HashMap::new(),
                            "new",
                            "u",
                            &mut sidecar,
                            &mut count,
                            &mut images,
                            &mut saved,
                            &mut modified,
                        );
                    } else if thinking {
                        prune_assistant_thinking(
                            &mut owned,
                            &opts,
                            "a",
                            &mut sidecar,
                            &mut count,
                            &mut saved,
                            &mut modified,
                        );
                    }
                    prune_declassify_text(
                        &mut owned,
                        &opts,
                        "new",
                        if user { "u" } else { "a" },
                        &mut sidecar,
                        &mut count,
                        &mut saved,
                        &mut modified,
                    );
                    drop_omitted_blocks(&mut owned);
                    let source = crate::json::JsonSource::capture(&path)?;
                    let plan = source_turn_prune(
                        &source,
                        Range {
                            offset: 0,
                            len: raw.len() as u64,
                        },
                        &opts,
                        "new",
                        0,
                        &HashMap::new(),
                        user,
                        thinking && !user,
                    )?;
                    if !modified {
                        assert!(plan.is_none());
                        continue;
                    }
                    let plan = plan.unwrap();
                    assert_eq!(
                        plan.payloads.len(),
                        count,
                        "case={case} drop={drop} thinking={thinking} min={min_hits}"
                    );
                    assert_eq!(plan.saved, saved);
                    assert_eq!(plan.images, images);
                    assert_eq!(plan.estimate, line_est(&owned));
                    let mut actual = Vec::new();
                    plan.record
                        .with_view(|view| view.write_canonical_replacing(&plan.replacements, &mut actual))?;
                    assert_eq!(
                        actual,
                        crate::restamp_line(&owned.to_string(), "new").unwrap().as_bytes()
                    );
                    if !drop {
                        let mut expected = Vec::new();
                        for entry in &sidecar {
                            serde_json::to_writer(&mut expected, entry)?;
                            expected.push(b'\n');
                        }
                        let mut actual = Vec::new();
                        for payload in &plan.payloads {
                            write_source_payload(payload, &mut actual)?;
                        }
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn source_declassify_matches_case_and_line_semantics_across_chunks() -> Result<()> {
    let world = crate::test_world("prune-declassify-chunks");
    let options = PruneOptions {
        declassify_tokens: vec![
            "alpha".into(),
            "beta".into(),
            "alphabeta".into(),
            "ALPHA".into(),
            "".into(),
            "alpha".into(),
            "ος".into(),
        ],
        ..Default::default()
    };
    for (index, text) in [
        format!("{}ALPHABETA\r\n{}\n", "x".repeat(65533), "x".repeat(65536)),
        format!("{} ΟΣ ΟΣΑ\r\n", "x".repeat(65536)),
        String::new(),
    ]
    .into_iter()
    .enumerate()
    {
        let path = world.base.join(format!("{index}.json"));
        let bytes = serde_json::to_vec(&text)?;
        std::fs::write(&path, &bytes)?;
        let json = crate::Json::source(&path, 0, bytes.len() as u64)?;
        json.with_view(|view| {
            let (stats, hits) = source_declassify_stats(Some(view), &options)?;
            let lower = text.to_lowercase();
            assert_eq!(
                hits,
                options
                    .declassify_tokens
                    .iter()
                    .filter(|token| !token.is_empty() && lower.contains(token.as_str()))
                    .count()
            );
            assert_eq!(stats.bytes, text.len());
            assert_eq!(stats.lines(), text.lines().count().max(1));
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn large_noop_declassification_preserves_raw_duplicate_ids_and_spacing() -> Result<()> {
    let world = crate::test_world("prune-noop-turn-source");
    let path = world.base.join("source.jsonl");
    let raw = format!("  {{\"type\":\"assistant\",\"sessionId\":\"first\",\"sessionId\":\"old\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}  ",
        serde_json::to_string(&"BENIGN prose\n".repeat(131072))?);
    std::fs::write(&path, format!("{raw}\r\n"))?;
    let result = prune_session(
        &path,
        &PruneOptions {
            declassify: true,
            declassify_tokens: vec!["alpha".into(), "beta".into()],
            new_id: Some("new".into()),
            ..Default::default()
        },
    )?;
    assert_eq!(result.pruned_count, 0);
    assert!(result.sidecar_path.is_none());
    let actual = std::fs::read(result.new_path)?;
    // Positioned JSONL framing keeps leading bytes and trims trailing record whitespace.
    let expected = format!("{}\n", raw.trim_end());
    assert!(actual == expected.as_bytes(), "no-op record spacing or CRLF changed");
    Ok(())
}

#[test]
fn source_usage_projection_matches_owned_order_and_iterations() -> Result<()> {
    let world = crate::test_world("prune-usage-projection");
    for (index, original_ctx) in [
        None,
        Some(Value::Null),
        Some(Value::String("keep".into())),
        Some(Value::from(7)),
    ]
    .into_iter()
    .enumerate()
    {
        let mut original = serde_json::json!({"z":"first","message":{"content":[{"type":"text","text":"keep body λ"}],"usage":{
            "output_tokens":8,"cache_read_input_tokens":4,"input_tokens":900,"cache_creation_input_tokens":3,
            "cache_creation":{"unknown":"keep","ephemeral_5m_input_tokens":2},
            "iterations":[null,{"type":"advisor_message","input_tokens":80},{"type":"compaction","input_tokens":90},
                {"type":"fallback_message","output_tokens":4,"input_tokens":700,"cache_read_input_tokens":5,"cache_creation_input_tokens":6},
                {"other":"still pinned"}],"unknown":{"a":[1,2,3]}
        }},"a":"last"});
        if let Some(value) = original_ctx {
            original["_cv_orig_ctx"] = value;
        }
        let raw =
            serde_json::to_string(&original)?.replace("\"z\":\"first\"", "\"z\":\"ignored duplicate\",\"z\":\"first\"");
        let path = world.base.join(format!("{index}.json"));
        std::fs::write(&path, &raw)?;
        let source = crate::Json::source(&path, 0, raw.len() as u64)?;
        let mut expected: Value = serde_json::from_str(&raw)?;
        if expected.get("_cv_orig_ctx").is_none() {
            expected["_cv_orig_ctx"] = Value::from(900);
        }
        pin_usage(
            expected.pointer_mut("/message/usage").unwrap().as_object_mut().unwrap(),
            31,
        );
        let mut actual = Vec::new();
        source.with_view(|view| write_source_usage(view, 900, 31, &mut actual))?;
        assert_eq!(actual, serde_json::to_vec(&expected)?);
    }
    Ok(())
}

#[test]
fn staged_usage_projection_preserves_mixed_raw_framing_and_source_guard() -> Result<()> {
    let world = crate::test_world("prune-usage-staged");
    let path = world.base.join("source.jsonl");
    let mut row = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"small"}],"usage":{"input_tokens":900,"output_tokens":7}}});
    let raw = format!("invalid {{ \r\n  {row}  \r\nlast raw row\n");
    std::fs::write(&path, &raw)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let mut stage = ByteStage::new();
    stage.write_all(raw.as_bytes())?;
    let facts = [None, Some(decision_facts(&row, true)), None];
    let (output, rewritten, honest, stale) =
        revive_staged(&source, stage.finish()?, &facts, &[false; 3], None, true, None)?;
    assert_eq!(rewritten, 1);
    assert_eq!(stale, Some(900));
    row["_cv_orig_ctx"] = Value::from(900);
    pin_usage(
        row.pointer_mut("/message/usage").unwrap().as_object_mut().unwrap(),
        honest.unwrap(),
    );
    let expected = format!("invalid {{ \r\n{row}\nlast raw row\n");
    let mut actual = Vec::new();
    output.reader()?.read_to_end(&mut actual)?;
    assert_eq!(actual, expected.as_bytes());
    let mut stage = ByteStage::new();
    stage.write_all(raw.as_bytes())?;
    std::fs::write(&path, b"changed source")?;
    assert!(revive_staged(&source, stage.finish()?, &facts, &[false; 3], None, true, None).is_err());
    Ok(())
}

#[test]
fn staged_usage_trim_preserves_whitespace_only_buffers_and_eof_failure() -> Result<()> {
    let world = crate::test_world("prune-usage-trim-buffers");
    let path = world.base.join("source.jsonl");
    let row = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"small"}],"usage":{"input_tokens":900,"output_tokens":7}}});
    let raw = format!("{}{}{}\r\n", " ".repeat(2 * 65536), row, "\t".repeat(2 * 65536));
    std::fs::write(&path, &raw)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let facts = [Some(decision_facts(&row, true))];
    let mut stage = ByteStage::new();
    stage.write_all(raw.as_bytes())?;
    let (output, rewritten, honest, _) = revive_staged(&source, stage.finish()?, &facts, &[false], None, true, None)?;
    assert_eq!(rewritten, 1);
    let mut expected = row;
    expected["_cv_orig_ctx"] = Value::from(900);
    pin_usage(
        expected.pointer_mut("/message/usage").unwrap().as_object_mut().unwrap(),
        honest.unwrap(),
    );
    let mut actual = Vec::new();
    output.reader()?.read_to_end(&mut actual)?;
    assert_eq!(actual, format!("{expected}\n").as_bytes());
    // The producer promises newline framing; truncated private rows still fail at EOF.
    let mut stage = ByteStage::new();
    stage.write_all(raw.trim_end_matches('\n').as_bytes())?;
    assert!(revive_staged(&source, stage.finish()?, &facts, &[false], None, true, None).is_err());
    Ok(())
}

#[test]
fn linked_tool_input_projection_matches_frozen_owned_lookup() -> Result<()> {
    fn owned_lookup(
        file: &std::fs::File,
        v: &Value,
        tools: &HashMap<String, (String, Range, usize)>,
    ) -> Result<HashMap<String, (String, Value)>> {
        let mut result = HashMap::new();
        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                if result.contains_key(id) {
                    continue;
                }
                if let Some((name, range, index)) = tools.get(id) {
                    let mut original: Value = serde_json::from_str(&read_line(file, *range)?)?;
                    let input = original
                        .pointer_mut(&format!("/message/content/{index}/input"))
                        .map(Value::take)
                        .unwrap_or(Value::Null);
                    result.insert(id.to_owned(), (name.clone(), input));
                }
            }
        }
        Ok(result)
    }
    let world = crate::test_world("prune-linked-input");
    let path = world.base.join("source.jsonl");
    let raw = format!(
        "{{\"type\":\"assistant\",\"ignored\":{},\"message\":{{\"content\":[\
        {{\"type\":\"thinking\",\"thinking\":\"unrelated\"}},\
        {{\"type\":\"tool_use\",\"id\":\"duplicate\",\"name\":\"first\",\"input\":{{\"a\":1,\"a\":2}}}},\
        {{\"type\":\"tool_use\",\"id\":\"missing\",\"name\":\"NoInput\"}},\
        {{\"type\":\"tool_use\",\"id\":\"duplicate\",\"name\":\"last\",\"input\":[{{\"keep\":\"λ\"}}]}}]}}}}",
        serde_json::to_string(&"x".repeat(2 * 1024 * 1024))?
    );
    std::fs::write(&path, &raw)?;
    let file = std::fs::File::open(&path)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let range = Range {
        offset: 0,
        len: raw.len() as u64,
    };
    let original: Value = serde_json::from_str(&raw)?;
    let mut tools = HashMap::new();
    collect_tool_names(&original, range, &mut tools);
    let user = serde_json::json!({"message":{"content":[
        {"type":"tool_result","tool_use_id":"duplicate"},{"type":"tool_result","tool_use_id":"duplicate"},
        {"type":"tool_result","tool_use_id":"missing"},{"type":"tool_result","tool_use_id":"unknown"},
        {"type":"tool_result","tool_use_id":17},{"type":"text","tool_use_id":"duplicate"}]}});
    let expected = owned_lookup(&file, &user, &tools)?;
    assert_eq!(materialize_tool_inputs(&source, &user, &tools)?, expected);
    assert_eq!(
        expected["duplicate"],
        ("last".into(), serde_json::json!([{"keep":"λ"}]))
    );
    assert_eq!(expected["missing"], ("NoInput".into(), Value::Null));
    assert!(!expected.contains_key("unknown"));
    let mut facts_tools = HashMap::new();
    source.read_range_value(0, range.len, |view| {
        decision_facts_source(view, range, &PruneOptions::default(), &mut facts_tools).map(|_| ())
    })?;
    assert_eq!(materialize_tool_inputs(&source, &user, &facts_tools)?, expected);
    std::fs::write(&path, b"unavailable original metadata")?;
    assert!(materialize_tool_inputs(&source, &user, &tools).is_err());
    assert!(owned_lookup(&file, &user, &tools).is_err());
    Ok(())
}

#[test]
fn unicode_sigma_classes_and_chunked_matching_equal_std() -> Result<()> {
    // Cased+ignorable overlaps must be ignored, not treated as word context.
    for character in ['\u{0345}', '\u{02b0}', '\u{200d}', '\u{00ad}', '\'', '.', ':', '`'] {
        assert_eq!(sigma_class(character), SigmaClass::Ignorable, "{character:?}");
    }
    for character in ['A', 'a', 'Σ', 'ς', 'ǅ', 'Ⅳ', 'µ'] {
        assert_eq!(sigma_class(character), SigmaClass::Cased, "{character:?}");
    }
    for character in [' ', '\n', '0', '🙂', '京'] {
        assert_eq!(sigma_class(character), SigmaClass::Boundary, "{character:?}");
    }
    let tokens: Vec<String> = [
        "",
        "Σ",
        "A",
        "σ",
        "ς",
        "σς",
        "σσ",
        "aσ",
        "aς",
        "aσ",
        "i\u{0307}",
        "σ\u{0345}",
        "ς\u{0345}",
        "aσ\u{0345}a",
        "aς\u{0345}a",
        "ǆς",
        "🙂 alpha",
        "beta",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    for text in [
        "🙂 ALPHA beta İ",
        "Σ",
        "AΣ",
        "ΣΣ",
        "AΣΣA",
        "\u{0345}Σ",
        "AΣ\u{0345}",
        "AΣ\u{0345}A",
        "AΣ\u{0345}AΣ",
        "AΣ\u{02b0} ",
        "ǅΣ",
        "AΣǅ",
        "ⅣΣ\u{200d}",
        "AΣ\u{00ad} ",
        "AΣ:\u{0345}.",
        "ΣAΣAΣ",
    ] {
        let lowered = text.to_lowercase();
        let expected = tokens
            .iter()
            .filter(|token| !token.is_empty() && lowered.contains(token.as_str()))
            .count();
        for cut in text.char_indices().map(|(offset, _)| offset).chain([text.len()]) {
            let mut matcher = SigmaMatcher::new(&tokens);
            matcher.push(&text[..cut]);
            matcher.push(&text[cut..]);
            assert_eq!(matcher.finish(), expected, "text={text:?}, cut={cut}");
        }
    }
    // Repeated sigmas reuse all three matched-bit allocations, even when choosing both branches.
    let mut matcher = SigmaMatcher::new(&tokens);
    let pointers = [
        matcher.current.matched.as_ptr(),
        matcher.normal.matched.as_ptr(),
        matcher.final_sigma.matched.as_ptr(),
    ];
    let repeated = "AΣ AΣA ".repeat(4096);
    matcher.push(&repeated);
    for pointer in [
        matcher.current.matched.as_ptr(),
        matcher.normal.matched.as_ptr(),
        matcher.final_sigma.matched.as_ptr(),
    ] {
        assert!(pointers.contains(&pointer));
    }
    let lowered = repeated.to_lowercase();
    assert_eq!(
        matcher.finish(),
        tokens
            .iter()
            .filter(|token| !token.is_empty() && lowered.contains(token.as_str()))
            .count()
    );
    Ok(())
}

#[test]
fn unicode_declassification_source_preserves_stats_and_pending_branches() -> Result<()> {
    let world = crate::test_world("prune-unicode-context");
    let options = PruneOptions {
        declassify_tokens: [
            "",
            "aσ",
            "aς",
            "aσ",
            "σ\u{0345}",
            "ς\u{0345}",
            "Σ",
            "i\u{0307}",
            "🙂 alpha",
            "beta",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        ..Default::default()
    };
    let ignorable = "\u{0345}\u{200d}\u{02b0}".repeat(32768);
    for (index, text) in [
        format!("{}🙂 ALPHA beta İ\r\n", "x".repeat(65533)),
        format!("AΣ{ignorable} \r\n"),
        format!("AΣ{ignorable}A\r\n"),
        format!("\u{0345}Σ{ignorable} ΣΣ\n"),
        format!("{}AΣ{}", "x".repeat(65535), "\u{0345}".repeat(65536)),
    ]
    .into_iter()
    .enumerate()
    {
        // Exercise literal UTF-8 and escaped JSON independently of callback framing.
        let literal = serde_json::to_string(&text)?;
        let escaped = literal
            .chars()
            .map(|c| {
                if c.is_ascii() {
                    c.to_string()
                } else {
                    let mut units = [0u16; 2];
                    c.encode_utf16(&mut units)
                        .iter()
                        .map(|u| format!("\\u{u:04x}"))
                        .collect::<String>()
                }
            })
            .collect::<String>();
        for (kind, bytes) in [("literal", literal), ("escaped", escaped)] {
            let path = world.base.join(format!("{index}-{kind}.json"));
            std::fs::write(&path, &bytes)?;
            let source = crate::Json::source(&path, 0, bytes.len() as u64)?;
            source.with_view(|view| {
                let (stats, hits) = source_declassify_stats(Some(view), &options)?;
                assert_eq!(stats.bytes, text.len());
                assert_eq!(stats.lines(), text.lines().count().max(1));
                assert_eq!(hits, declassify_hits(&text, &options), "case={index}/{kind}");
                Ok(())
            })?;
        }
    }
    Ok(())
}

#[test]
fn sigma_insensitive_tokens_keep_exact_hits_across_context_boundaries() -> Result<()> {
    let world = crate::test_world("prune-sigma-demand");
    let options = PruneOptions {
        declassify_tokens: ["alpha", "βeta", "🙂", "Σ", "A", "", "alpha", "Alpha"]
            .into_iter()
            .map(String::from)
            .collect(),
        ..Default::default()
    };
    let text = format!(
        "{}AΣ{} AΣA ALPHA βeta 🙂\r\n",
        "x".repeat(65535),
        "\u{0345}".repeat(65536)
    );
    let bytes = serde_json::to_vec(&text)?;
    let path = world.base.join("source.json");
    std::fs::write(&path, &bytes)?;
    let source = crate::Json::source(&path, 0, bytes.len() as u64)?;
    source.with_view(|view| {
        let (stats, hits) = source_declassify_stats(Some(view), &options)?;
        assert_eq!(hits, declassify_hits(&text, &options));
        assert_eq!(stats.bytes, text.len());
        assert_eq!(stats.lines(), text.lines().count().max(1));
        Ok(())
    })?;
    Ok(())
}

#[test]
fn sigma_context_batches_long_ordinary_runs_without_changing_matches() {
    let tokens = vec!["alpha".into(), "ος".into(), "σx".into(), "ςx".into(), "ς ".into()];
    for text in [
        format!("ALPHA {}ΟΣ ", "x".repeat(1 << 20)),
        format!("AΣ{}AΣ ", "x".repeat(1 << 20)),
        format!("AΣ{}Σ ", "'".repeat(1 << 17)),
    ] {
        let lowered = text.to_lowercase();
        let expected = tokens
            .iter()
            .filter(|token: &&String| !token.is_empty() && lowered.contains(token.as_str()))
            .count();
        for chunk_size in [65535, 65536, 65537] {
            let mut matcher = SigmaMatcher::new(&tokens);
            let mut rest = text.as_str();
            while !rest.is_empty() {
                let mut end = rest.len().min(chunk_size);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                matcher.push(&rest[..end]);
                rest = &rest[end..];
            }
            assert_eq!(matcher.finish(), expected);
        }
    }
}

#[test]
fn malformed_large_rows_preserve_owned_passthrough_and_fatal_utf8() -> Result<()> {
    let dir = tmpdir();
    let source = dir.join("source.jsonl");
    let prefix = " malformed sessionId=old ";
    let malformed = format!(
        "{prefix}{}[PRUNED id=ignored] {}🙂  \r",
        "x".repeat(65535 - prefix.len() - 5),
        "x".repeat(2 << 20)
    );
    let valid = r#" {"sessionId":"old","type":"user","uuid":"u","message":{"content":"kept"}} "#;
    let text = format!("{malformed}\n{valid}\r\n\u{2003} \r\n");
    std::fs::write(&source, &text)?;
    // Frozen legacy acceptance/framing: global trim_end, per-line serde Value acceptance,
    // invalid JSON passthrough, id surgery only on valid rows, one LF per retained record.
    let expected: String = text
        .trim_end()
        .split('\n')
        .map(|line| {
            let line = if serde_json::from_str::<Value>(line).is_ok() {
                crate::restamp_line(line, "new").unwrap()
            } else {
                line.to_owned()
            };
            line + "\n"
        })
        .collect();
    for revive in [false, true] {
        let result = prune_session(
            &source,
            &PruneOptions {
                new_id: Some("new".into()),
                revive,
                force: true,
                ..Default::default()
            },
        )?;
        assert_eq!(std::fs::read_to_string(&result.new_path)?, expected);
        assert_eq!(result.source_id, "old");
        assert_eq!(result.pruned_count, 0);
        assert!(result.sidecar_path.is_none());
        assert_eq!(std::fs::read_to_string(&source)?, text);
    }
    let previous = std::fs::read(dir.join("new.jsonl"))?;
    let mut invalid = text.into_bytes();
    invalid[1 << 20] = 0xff;
    std::fs::write(&source, invalid)?;
    let error = prune_session(
        &source,
        &PruneOptions {
            new_id: Some("new".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(error.to_string(), "stream did not contain valid UTF-8");
    assert_eq!(std::fs::read(dir.join("new.jsonl"))?, previous);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn rejected_large_json_keeps_strict_owned_value_acceptance() -> Result<()> {
    let dir = tmpdir();
    let path = dir.join("source.jsonl");
    let padding = " ".repeat(LARGE_RECORD as usize);
    for raw in [
        "{bad}".to_owned(),
        "1e400".to_owned(),
        r#"{"sessionId":"old","ignored":1e400}"#.to_owned(),
        r#""\ud800""#.to_owned(),
        r#"{"sessionId":"old","ignored":"\ud800"}"#.to_owned(),
        format!("{}0{}", "[".repeat(130), "]".repeat(130)),
        r#"{"sessionId":"old","type":"user","message":{"content":"kept"}}"#.to_owned(),
    ] {
        let row = format!("{padding}{raw}\n");
        std::fs::write(&path, &row)?;
        let accepted = serde_json::from_str::<Value>(&row).is_ok();
        let result = prune_session(
            &path,
            &PruneOptions {
                new_id: Some("new".into()),
                force: true,
                ..Default::default()
            },
        )?;
        let expected = if accepted {
            crate::restamp_line(row.trim_end(), "new").unwrap() + "\n"
        } else {
            row.clone()
        };
        assert_eq!(std::fs::read_to_string(result.new_path)?, expected);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

/// Native children carry their parent's sessionId and sidechain flags on every own turn.
fn native_child_fixture(base: &Path, big: &str) -> Result<(PathBuf, PathBuf, Vec<u8>)> {
    let project = base.join("projects/fixture");
    std::fs::create_dir_all(&project)?;
    let original = fixture(&project, big);
    let before = std::fs::read(&original)?;
    let parent = original.file_stem().unwrap().to_str().unwrap();
    let subdir = project.join(parent).join("subagents");
    std::fs::create_dir_all(&subdir)?;
    let child = subdir.join("agent-native-child.jsonl");
    let mut rows = String::new();
    for row in std::str::from_utf8(&before)?.lines() {
        let mut value: Value = serde_json::from_str(row)?;
        value["isSidechain"] = Value::Bool(true);
        value["agentId"] = Value::String("native-child".into());
        if value["uuid"] == "u0" {
            value["parentUuid"] = Value::String("parent-turn".into());
        }
        rows.push_str(&value.to_string());
        rows.push('\n');
    }
    std::fs::write(&child, rows)?;
    Ok((original, child, before))
}

#[test]
fn subagent_prune_counts_own_turns_and_emits_standalone_without_touching_parent() -> Result<()> {
    let world = crate::test_world("prune-native-child");
    for size in [4096, 1100000] {
        let base = world.base.join(size.to_string());
        let (parent, child, parent_before) = native_child_fixture(&base, &"x".repeat(size))?;
        let child_before = std::fs::read(&child)?;
        let opts = PruneOptions {
            keep_last: 2,
            min_size: 100,
            new_id: Some("child-pruned".into()),
            ..Default::default()
        };
        // Child resources are distinct from its parent's other children/workflows.
        let parent_resources = parent.with_extension("");
        std::fs::write(
            parent_resources.join("unrelated-parent-resource"),
            "leave parent resources alone",
        )?;
        let child_resources = child.with_extension("");
        std::fs::create_dir_all(&child_resources)?;
        std::fs::write(child_resources.join("child-resource"), "own")?;
        let result = prune_session(&child, &opts)?;
        assert_eq!(result.source_id, "agent-native-child");
        assert_eq!(
            result.parent_session_id.as_deref(),
            parent.file_stem().and_then(|x| x.to_str())
        );
        assert_eq!(
            result.pruned_count, 2,
            "old child result and mirror are snipped; recent result survives"
        );
        assert_eq!(result.new_path.parent(), parent.parent());
        let output = std::fs::read_to_string(&result.new_path)?;
        let rows: Vec<Value> = output
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        for row in &rows {
            assert_eq!(row["sessionId"], "child-pruned");
            assert_eq!(row["isSidechain"], false);
            assert!(row.get("agentId").is_none());
        }
        assert!(rows[0]["parentUuid"].is_null());
        assert!(rows[2]["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains(MARKER_PREFIX));
        assert_eq!(rows[4]["message"]["content"][0]["content"], "x".repeat(size));
        assert_eq!(std::fs::read(&parent)?, parent_before);
        assert_eq!(std::fs::read(&child)?, child_before);
        let copied = prune_session(
            &child,
            &PruneOptions {
                copy_resources: true,
                new_id: Some("child-resources-pruned".into()),
                ..opts.clone()
            },
        )?;
        let resources = copied.copied_resources.unwrap();
        assert!(resources.join("child-resource").is_file());
        assert!(!resources.join("unrelated-parent-resource").exists());
        let mut collision = opts.clone();
        collision.new_id = result.parent_session_id;
        assert!(prune_session(&child, &collision).is_err());
        assert_eq!(std::fs::read(&parent)?, parent_before);
    }
    Ok(())
}

#[test]
fn subagent_prune_keep_window_and_usage_match_standalone_turn_semantics() -> Result<()> {
    let world = crate::test_world("prune-native-child-window");
    let (parent, child, _) = native_child_fixture(&world.base, &"x".repeat(4096))?;
    for (label, keep_range, window) in [("keep", Some((3, None)), None), ("window", None, Some(20))] {
        let opts = PruneOptions {
            keep_range,
            window,
            new_id: Some(format!("child-{label}")),
            ..Default::default()
        };
        let expected = prune_session(
            &parent,
            &PruneOptions {
                new_id: Some(format!("parent-{label}")),
                ..opts.clone()
            },
        )?;
        let actual = prune_session(&child, &opts)?;
        assert!(actual.dropped_turns > 0);
        assert_eq!(actual.dropped_turns, expected.dropped_turns);
        let normalized = |path: &Path| -> Result<Vec<Value>> {
            std::fs::read_to_string(path)?
                .lines()
                .map(|line| {
                    let mut value: Value = serde_json::from_str(line)?;
                    if let Some(object) = value.as_object_mut() {
                        object.remove("sessionId");
                        object.remove("session_id");
                        object.remove("isSidechain");
                    }
                    Ok(value)
                })
                .collect()
        };
        assert_eq!(normalized(&actual.new_path)?, normalized(&expected.new_path)?);
    }
    // Provider-recorded usage belongs to this selected child, rather than an embedded sidechain.
    let mut rows: Vec<Value> = std::fs::read_to_string(&child)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    for (index, row) in rows.iter_mut().enumerate() {
        if row["type"] == "assistant" {
            row["message"]["usage"] = serde_json::json!({"input_tokens": (index + 1) * 100});
        }
    }
    std::fs::write(&child, rows.iter().map(|v| format!("{v}\n")).collect::<String>())?;
    let actual = prune_session(
        &child,
        &PruneOptions {
            window: Some(250),
            revive: true,
            new_id: Some("child-usage".into()),
            ..Default::default()
        },
    )?;
    assert!(actual.window_real_tokens.is_some());
    assert!(actual.dropped_turns > 0);
    Ok(())
}

#[test]
fn parent_prune_keeps_embedded_sidechains_out_of_own_turns() -> Result<()> {
    let world = crate::test_world("prune-embedded-child");
    let (parent, child, before) = native_child_fixture(&world.base, &"x".repeat(4096))?;
    let embedded = std::fs::read(&child)?;
    let mut mixed = before;
    mixed.extend_from_slice(&embedded);
    std::fs::write(&parent, &mixed)?;
    let result = prune_session(
        &parent,
        &PruneOptions {
            keep_last: 2,
            min_size: 100,
            new_id: Some("parent-pruned".into()),
            ..Default::default()
        },
    )?;
    assert!(result.parent_session_id.is_none());
    let rows: Vec<Value> = std::fs::read_to_string(&result.new_path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert!(rows.last().unwrap()["isSidechain"].as_bool().unwrap());
    assert_eq!(rows.last().unwrap()["agentId"], "native-child");
    assert_eq!(std::fs::read(parent)?, mixed);
    Ok(())
}

#[test]
fn prune_rejects_output_ids_that_escape_or_alias_resource_directories() {
    let dir = tmpdir();
    let source = fixture(&dir, "original payload");
    let original = std::fs::read(&source).unwrap();
    for id in [
        "",
        ".",
        "..",
        "../victim",
        "/tmp/victim",
        "nested/child",
        "nested\\child",
        "bad\0id",
    ] {
        let error = prune_session(
            &source,
            &PruneOptions {
                new_id: Some(id.into()),
                copy_resources: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("filename component"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// `--copy-resources` never merges into an existing resource directory: it is refused, and with
/// `force` the new directory replaces it whole (stale entries gone, a symlink replaced, not followed).
#[test]
fn copied_resources_are_refused_or_replaced_whole_never_merged() {
    let dir = tmpdir();
    let source = fixture(&dir, &"payload".repeat(1024));
    let resources = dir.join(source.file_stem().unwrap()).join("subagents");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(resources.join("agent-a.jsonl"), b"agent transcript").unwrap();
    let new_id = "22222222-2222-4222-8222-222222222222";
    let destination = dir.join(new_id);
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("stale.txt"), b"another session's resource").unwrap();
    let mut options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        copy_resources: true,
        ..Default::default()
    };
    let error = prune_session(&source, &options).unwrap_err();
    assert!(error.downcast_ref::<crate::publish::Exists>().is_some(), "{error:#}");
    assert!(
        !dir.join(format!("{new_id}.jsonl")).exists(),
        "a refused prune writes nothing"
    );
    assert_eq!(
        std::fs::read(destination.join("stale.txt")).unwrap(),
        b"another session's resource"
    );
    options.force = true;
    prune_session(&source, &options).unwrap();
    assert!(!destination.join("stale.txt").exists(), "replaced, not merged");
    assert_eq!(
        std::fs::read(destination.join("subagents/agent-a.jsonl")).unwrap(),
        b"agent transcript"
    );
    assert_eq!(
        std::fs::read(resources.join("agent-a.jsonl")).unwrap(),
        b"agent transcript"
    );
    #[cfg(unix)]
    {
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.txt"), b"kept").unwrap();
        std::fs::remove_dir_all(&destination).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &destination).unwrap();
        prune_session(&source, &options).unwrap();
        assert!(!std::fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(elsewhere.join("keep.txt")).unwrap(), b"kept");
    }
    assert!(!std::fs::read_dir(&dir)
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".cv-")));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resource_destination_that_contains_the_source_is_refused() {
    let dir = tmpdir();
    let source = dir.join("outer/source-resources");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("agent.jsonl"), b"original resource").unwrap();
    assert!(stage_resources(&source, &dir.join("outer")).is_err());
    assert_eq!(std::fs::read(source.join("agent.jsonl")).unwrap(), b"original resource");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn byte_window_accounts_for_trailing_reminders_and_prompt_snap_overshoot() {
    let options = PruneOptions {
        window: Some(100),
        ..Default::default()
    };
    let message = |role: &str, text: &str| serde_json::json!({"type":role,"message":{"content":text}});
    let rows = vec![
        Some(message("user", "first")),
        Some(message("assistant", "answer")),
        Some(message("user", "latest")),
        Some(message("assistant", "reply")),
        Some(serde_json::json!({"type":"attachment","rendered":[{"content":"x".repeat(1000)}]})),
    ];
    assert_eq!(select_kept_turns(&rows, &options, 4), (2, 4, None, true));
    let rows = vec![
        Some(message("user", "first")),
        Some(message("assistant", "answer")),
        Some(message("user", &"x".repeat(1000))),
        Some(message("assistant", "reply")),
    ];
    assert_eq!(select_kept_turns(&rows, &options, 4), (2, 4, None, true));
    let options = PruneOptions {
        window: Some(1000),
        ..options
    };
    assert_eq!(select_kept_turns(&rows, &options, 4), (0, 4, None, false));
}
