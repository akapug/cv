use super::*;
use serde_json::json;

const SID: &str = "11111111-1111-4111-8111-111111111111";
const SHA: &str = "63cef473b771e07a2b5060e50373a41a27d54c0e";

/// A temp `projects/`-style dir named the way Claude files sessions launched from `/work/proj`.
fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir()
        .join(format!("cv-rewind-test-{}", uuid::Uuid::new_v4()))
        .join("-work-proj");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cleanup(dir: &Path) {
    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
}

fn write_jsonl(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    std::fs::write(path, body).unwrap();
}

fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn rec(ty: &str, uuid: &str, parent: Option<&str>, message: Value) -> Value {
    json!({"type": ty, "sessionId": SID, "uuid": uuid, "parentUuid": parent, "isSidechain": false,
           "cwd": "/work/proj", "timestamp": "2026-06-16T00:00:00Z", "message": message})
}

fn bash(id: &str, cmd: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": cmd}}]})
}

fn result(id: &str, out: &str) -> Value {
    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": out}]})
}

fn text(role: &str, t: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": t}]})
}

/// A two-compaction session shaped like the real ones (1-based source lines):
///
/// ```text
///  1 prompt               7 result "[main 63cef47]"    13 result t1
///  2 `cat` call           8 assistant a2 "landed"      14 hook attachment
///  3 decoy: sha in cat    9 boundary b2 (preserves a2) 15 result t2
///  4 boundary b1         10 summary s2                 16 assistant a5 "done"
///  5 summary s1          11 Read t1 ┐ parallel         17 prompt "next thing"
///  6 git commit (a1)     12 Read t2 ┘
/// ```
fn session(dir: &Path) -> PathBuf {
    let lines = vec![
        rec(
            "user",
            "u0",
            None,
            json!({"role": "user", "content": "land the gate fix"}),
        ),
        rec("assistant", "d0", Some("u0"), bash("toolu_d", "cat notes.txt")),
        // Mentions the sha, but `cat` is no commit — must never count as evidence.
        rec(
            "user",
            "d1",
            Some("d0"),
            result("toolu_d", &format!("todo: review {SHA}")),
        ),
        json!({"type": "system", "subtype": "compact_boundary", "sessionId": SID, "uuid": "b1",
               "parentUuid": null, "logicalParentUuid": "d1", "isSidechain": false,
               "compactMetadata": {"trigger": "auto", "preTokens": 900000}}),
        json!({"type": "user", "sessionId": SID, "uuid": "s1", "parentUuid": "b1", "isSidechain": false,
               "isCompactSummary": true, "message": {"role": "user", "content": "Summary: landing the gate fix"}}),
        rec(
            "assistant",
            "a1",
            Some("s1"),
            bash("toolu_c", "cd /work/proj && git add -A && git commit -m 'gate fix'"),
        ),
        rec(
            "user",
            "r1",
            Some("a1"),
            result("toolu_c", "[main 63cef47] gate fix\n 1 file changed"),
        ),
        rec("assistant", "a2", Some("r1"), text("assistant", "landed 63cef47")),
        json!({"type": "system", "subtype": "compact_boundary", "sessionId": SID, "uuid": "b2",
               "parentUuid": null, "logicalParentUuid": "a2", "isSidechain": false,
               "compactMetadata": {"trigger": "auto",
                   "preservedSegment": {"headUuid": "a2", "anchorUuid": "s2", "tailUuid": "a2"}}}),
        json!({"type": "user", "sessionId": SID, "uuid": "s2", "parentUuid": "b2", "isSidechain": false,
               "isCompactSummary": true, "message": {"role": "user", "content": "Summary: gate fix landed"}}),
        rec(
            "assistant",
            "t1u",
            Some("s2"),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/a"}}]}),
        ),
        rec(
            "assistant",
            "t2u",
            Some("t1u"),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/b"}}]}),
        ),
        rec("user", "t1r", Some("t2u"), result("t1", "contents of a")),
        json!({"type": "attachment", "sessionId": SID, "uuid": "h1", "parentUuid": "t1r", "isSidechain": false,
               "attachment": {"type": "hook_success", "content": "ok"}, "rendered": [{"content": "hook ok"}]}),
        rec("user", "t2r", Some("h1"), result("t2", "contents of b")),
        rec("assistant", "a5", Some("t2r"), text("assistant", "done")),
        rec(
            "user",
            "u9",
            Some("a5"),
            json!({"role": "user", "content": "next thing"}),
        ),
    ];
    let path = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&path, &lines);
    path
}

fn sref(path: &Path, id: &str) -> SessionRef {
    let r = SessionRef::from_path(path, Harness::Claude);
    assert_eq!(r.id, id, "a fixture's id is its file stem");
    r
}

/// The records a real transcript ends with AFTER its last message — none of them a message, and
/// none carrying a `uuid`. The measured defect: `cv rewind --full` of a 238-record session wrote
/// 235, dropping exactly these three shapes.
fn tail_records() -> Vec<Value> {
    vec![
        json!({"type": "bridge-session", "sessionId": SID, "bridgeSessionId": "cse_1", "lastSequenceNum": 0}),
        json!({"type": "history-suppression", "sessionId": SID, "cause": "env_owner_mismatch", "ts": "2026-10-01T22:25:24.554Z"}),
        json!({"type": "cost-state", "sessionId": SID, "totalCostUSD": 0, "totalLinesAdded": 0}),
    ]
}

/// A prompt, its answer, then the tail above: two messages, five records.
fn session_tailed(dir: &Path) -> PathBuf {
    let mut lines = vec![
        rec("user", "u0", None, text("user", "land the gate fix")),
        rec("assistant", "a0", Some("u0"), text("assistant", "landed")),
    ];
    lines.extend(tail_records());
    let path = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&path, &lines);
    path
}

/// The synthetic case from the measured evidence (task7336 `critical-tail`): two messages, an
/// `invoked_skills` attachment record whose `rendered` is a string (so the codec surfaces no
/// message for it), then — with `malformed` — a record with a `type` claiming a turn and no
/// `message` body. `cv show` counts 2 messages and stops at the prompt.
fn session_attachment_and_bad_tail(dir: &Path, malformed: bool) -> PathBuf {
    let mut lines = vec![
        rec("user", "u0", None, text("user", "go")),
        rec("assistant", "a0", Some("u0"), text("assistant", "done")),
        json!({"type": "attachment", "uuid": "at1", "parentUuid": "a0", "sessionId": SID,
               "cwd": "/work/proj", "isSidechain": false,
               "attachment": {"type": "invoked_skills", "skills": [
                   {"name": "fresh", "path": "userSettings:fresh", "content": "the amber key"}]},
               "rendered": "padding"}),
    ];
    if malformed {
        lines.push(json!({"type": "user", "sessionId": SID, "uuid": "bad1"}));
    }
    let path = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&path, &lines);
    path
}

fn ty(v: &Value) -> &str {
    v["type"].as_str().unwrap()
}

/// The stream index (the `cv show --range` counting) of the message with this uuid.
fn idx_of(path: &Path, uuid: &str) -> usize {
    let s = crate::harness::claude::parse_str("x", &std::fs::read_to_string(path).unwrap(), Some(path.to_path_buf()));
    s.messages
        .iter()
        .position(|m| m.id.as_deref() == Some(uuid))
        .unwrap_or_else(|| panic!("no message {uuid}"))
}

fn opts(at: CutAt, out: &Path) -> RewindOptions {
    RewindOptions {
        at,
        full: false,
        out_dir: Some(out.to_path_buf()),
        new_id: Some("22222222-2222-4222-8222-222222222222".into()),
        dry_run: false,
        generator: "cv-test".into(),
        thinking: Some(crate::emit::ThinkingMode::Native),
        skills_policy: SkillsPolicy::Keep,
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn uuids(lines: &[Value]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|v| v.get("uuid").and_then(Value::as_str))
        .collect()
}

/// Walk `parentUuid` back from the last record: the chain Claude Code loads on resume. Returns the
/// uuids leaf-first, panicking on a dangling parent.
fn chain(lines: &[Value]) -> Vec<String> {
    let by: HashMap<&str, &Value> = lines
        .iter()
        .filter_map(|v| Some((v.get("uuid")?.as_str()?, v)))
        .collect();
    let mut out = Vec::new();
    let mut cur = lines.iter().rev().find(|v| v.get("uuid").is_some()).unwrap();
    loop {
        out.push(cur["uuid"].as_str().unwrap().to_string());
        match cur.get("parentUuid").and_then(Value::as_str) {
            None => return out,
            Some(p) => cur = by.get(p).unwrap_or_else(|| panic!("dangling parent {p}")),
        }
    }
}

#[test]
fn cut_at_message_index_starts_at_the_last_boundary_before_it() {
    let dir = tmpdir();
    let src = session(&dir);
    let out = dir.join("out");
    let cut = idx_of(&src, "a2");
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &out)).unwrap();

    // Window = boundary b1 (line 4) through a2 (line 8): the context the agent had at a2.
    assert_eq!((res.start_line, res.cut_line, res.end_line), (4, 8, 8));
    assert_eq!(res.boundary_msg_idx, Some(idx_of(&src, "b1")));
    assert_eq!(res.start_msg_idx, idx_of(&src, "b1"));
    assert!(!res.preserved_head);
    let got = read_lines(&res.new_path);
    assert_eq!(uuids(&got), ["b1", "s1", "a1", "r1", "a2"]);
    assert_eq!(res.lines_written, 5);
    // Every record carries the new id; parent links are exactly the source's.
    assert!(got
        .iter()
        .all(|v| v["sessionId"] == "22222222-2222-4222-8222-222222222222"));
    assert_eq!(chain(&got), ["a2", "r1", "a1", "s1", "b1"]);
    // The future is omitted, and said so.
    assert_eq!(res.omitted_lines, 9);
    assert_eq!(res.cwd.as_deref(), Some("/work/proj"));

    // A cut ON the later boundary's window starts at that boundary — never at an earlier one.
    let later = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            new_id: Some("33333333-3333-4333-8333-333333333333".into()),
            ..opts(CutAt::Message(idx_of(&src, "s2")), &out)
        },
    )
    .unwrap();
    assert_eq!(later.boundary_msg_idx, Some(idx_of(&src, "b2")));

    // Out of range is a clear error, not a silent clamp.
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::Message(999), &dir.join("x"))).unwrap_err();
    assert!(err.to_string().contains("message(s)"), "{err}");
    cleanup(&dir);
}

#[test]
fn cut_at_commit_sha_lands_on_the_tool_result_that_made_it() {
    let dir = tmpdir();
    let src = session(&dir);
    let out = dir.join("out");
    // The full sha resolves through the 7-digit `[main 63cef47]` the commit printed — and the
    // earlier `cat` result that merely mentions the sha is no evidence.
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Commit(SHA.into()), &out)).unwrap();
    assert_eq!(res.cut_line, 7, "{res:?}");
    assert_eq!(res.cut_msg_idx, idx_of(&src, "r1"));
    let ev = res.evidence.as_ref().unwrap();
    assert_eq!(ev.kind, EvidenceKind::Created);
    assert_eq!(ev.tool_use_id, "toolu_c");
    assert!(ev.command.contains("git commit"));
    assert_eq!(uuids(&read_lines(&res.new_path)), ["b1", "s1", "a1", "r1"]);

    // A short sha works the same way.
    let short = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            dry_run: true,
            ..opts(CutAt::Commit("63CEF47".into()), &out)
        },
    )
    .unwrap();
    assert_eq!(short.cut_line, 7);

    // No evidence → a clear failure naming the sha, nothing written.
    let none = dir.join("none");
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::Commit("abcdef1234".into()), &none)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("abcdef1234") && msg.contains("git commit"), "{msg}");
    assert!(!none.exists());
    cleanup(&dir);
}

#[test]
fn from_compaction_takes_the_preserved_segment_and_full_takes_everything() {
    let dir = tmpdir();
    let src = session(&dir);
    let out = dir.join("out");
    let cut = idx_of(&src, "a5");
    // b2 preserved a2 (line 8, just before the boundary): the start moves back to it.
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &out)).unwrap();
    assert!(res.preserved_head);
    assert_eq!((res.start_line, res.cut_line), (8, 16));
    assert_eq!(res.start_msg_idx, idx_of(&src, "a2"));
    assert_eq!(uuids(&read_lines(&res.new_path))[..3], ["a2", "b2", "s2"]);

    let full = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            full: true,
            new_id: Some("44444444-4444-4444-8444-444444444444".into()),
            ..opts(CutAt::Message(cut), &out)
        },
    )
    .unwrap();
    assert_eq!(
        (full.start_line, full.start_mode, full.boundary_msg_idx),
        (1, "full", None)
    );
    assert_eq!(full.lines_written, 16);
    cleanup(&dir);
}

/// ARM 1 (real tail) — `CutAt::End` copies through the END OF THE FILE, not through the last
/// message's record: the records after it ride along, with the same `sessionId` rewrite as the rest
/// of the copy. If the cure were wrong — still cutting at the last message — the output would end at
/// the answer and the tail would be gone, which is the measured 238 → 235 exactly.
#[test]
fn end_copies_the_records_after_the_last_message() {
    let dir = tmpdir();
    let src = session_tailed(&dir);
    let before = std::fs::read(&src).unwrap();
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("out"))).unwrap();

    let got = read_lines(&res.new_path);
    assert_eq!(
        got.iter().map(ty).collect::<Vec<_>>(),
        [
            "user",
            "assistant",
            "bridge-session",
            "history-suppression",
            "cost-state"
        ]
    );
    // The tail is the source's records, restamped and nothing else.
    assert_eq!(got[2]["bridgeSessionId"], "cse_1");
    assert_eq!(got[3]["cause"], "env_owner_mismatch");
    assert_eq!(got[4]["totalCostUSD"], 0);
    assert!(got
        .iter()
        .all(|v| v["sessionId"] == "22222222-2222-4222-8222-222222222222"));
    // Nothing was left behind: the copy runs to the end of the source as read.
    assert_eq!((res.cut_line, res.end_line, res.lines_written), (2, 5, 5));
    assert_eq!((res.omitted_lines, res.omitted_bytes), (0, 0));
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");
    cleanup(&dir);
}

/// CONTROL for arm 1 — an explicit cut is still a truncation: `--at <last message>` ends at that
/// message's record and copies none of the tail. If the cure were wrong in the other direction —
/// `End`'s full copy leaking into every cut — this would carry the tail too, and `omitted_lines`
/// would be 0 instead of 3.
#[test]
fn an_explicit_cut_still_ends_at_its_message_record() {
    let dir = tmpdir();
    let src = session_tailed(&dir);
    let cut = idx_of(&src, "a0"); // the last message
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &dir.join("out"))).unwrap();

    let got = read_lines(&res.new_path);
    assert_eq!(got.iter().map(ty).collect::<Vec<_>>(), ["user", "assistant"]);
    assert_eq!((res.cut_line, res.end_line), (2, 2));
    assert_eq!(res.omitted_lines, 3);
    cleanup(&dir);
}

/// ARM 2 (attachment + malformed tail) — a record the codec reads nothing out of (a `user` record
/// with no `message` body, the synthetic `critical-tail` shape) makes the rewind REFUSE, naming the
/// record, instead of truncating at the last message. If the cure were wrong — walking past it
/// silently — this would exit 0 having dropped the record and everything after it, which is the
/// measured 10 records in → 8 out.
#[test]
fn a_record_the_codec_cannot_read_refuses_instead_of_truncating() {
    let dir = tmpdir();
    let src = session_attachment_and_bad_tail(&dir, true);
    let before = std::fs::read(&src).unwrap();
    let out = dir.join("out");
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("line 4"), "{msg}");
    assert!(msg.contains("`user`") && msg.contains("no `message` body"), "{msg}");
    assert!(
        msg.contains("bad1"),
        "the refusal names the record it could not read: {msg}"
    );
    // Nothing was published, and no staged copy was left behind.
    assert!(!out.join("22222222-2222-4222-8222-222222222222.jsonl").exists());
    assert!(!out.join("22222222-2222-4222-8222-222222222222.rewind.json").exists());
    assert!(std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cv-rewind-")));
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");
    cleanup(&dir);
}

/// CONTROL for arm 2 — the same source with the unreadable record REMOVED copies the attachment:
/// the refusal comes from that record and not from the attachment, nor from the tail as such. If
/// the cure were wrong (refusing on any non-message tail record), this would fail; if it were wrong
/// the other way (still cutting at the last message), the attachment would be missing here.
#[test]
fn without_the_unreadable_record_the_attachment_rides_along() {
    let dir = tmpdir();
    let src = session_attachment_and_bad_tail(&dir, false);
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("out"))).unwrap();

    let got = read_lines(&res.new_path);
    assert_eq!(
        got.iter().map(ty).collect::<Vec<_>>(),
        ["user", "assistant", "attachment"]
    );
    assert_eq!(got[2]["attachment"]["skills"][0]["name"], "fresh");
    assert_eq!(got[2]["rendered"], "padding");
    assert_eq!(got[2]["sessionId"], "22222222-2222-4222-8222-222222222222");
    assert_eq!(res.omitted_lines, 0);
    cleanup(&dir);
}

#[test]
fn end_refuses_invalid_json_tail_in_native_and_staged_copies() {
    for (thinking, skills_policy) in [
        (crate::emit::ThinkingMode::Native, SkillsPolicy::Keep),
        (crate::emit::ThinkingMode::Native, SkillsPolicy::DropAll),
        (crate::emit::ThinkingMode::Portable, SkillsPolicy::Keep),
    ] {
        for tail in ["{\"type\":\"cost-state\"", "not-json\n", "{\"type\":}\n", "\u{000c}\n"] {
            let dir = tmpdir();
            let src = session_tailed(&dir);
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all(tail.as_bytes())
                .unwrap();
            let before = std::fs::read(&src).unwrap();
            let out = dir.join("out");
            let options = RewindOptions {
                thinking: Some(thinking),
                skills_policy,
                ..opts(CutAt::End, &out)
            };
            let result = rewind_session(&sref(&src, SID), &options);
            assert!(
                result.is_err(),
                "invalid tail published under {thinking:?}/{skills_policy:?}: {tail:?}"
            );
            assert!(std::fs::read_dir(&out).unwrap().next().is_none());
            assert_eq!(std::fs::read(&src).unwrap(), before);
            // A deliberate cut need not validate the bytes it omits.
            let cut = rewind_session(
                &sref(&src, SID),
                &RewindOptions {
                    at: CutAt::Message(1),
                    ..options
                },
            )
            .unwrap();
            assert_eq!(cut.lines_written, 2);
            assert_eq!(cut.omitted_lines, 4);
            cleanup(&dir);
        }
    }
}

/// An explicit cut whose message leaves a call open scans on past the cut (`Closing`) for the
/// result. A malformed line on the way is a future record the cut never meant to copy: it is
/// omitted, and the result after it still closes the call. The same bytes under `End` sit inside
/// the copied window and refuse, so the omission is the cut's, not a relaxed parse.
#[test]
fn an_explicit_cut_omits_a_malformed_line_while_it_closes_an_open_call() {
    let dir = tmpdir();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec("assistant", "a0", Some("u0"), bash("t1", "cat notes")),
        ],
    );
    let mut tail = b"not-json\n".to_vec();
    for v in [
        rec("user", "t1r", Some("a0"), result("t1", "notes")),
        rec("assistant", "a1", Some("t1r"), text("assistant", "read")),
    ] {
        tail.extend(v.to_string().into_bytes());
        tail.push(b'\n');
    }
    append(&src, &tail);
    let cut = idx_of(&src, "a0");

    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &dir.join("out"))).unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0", "t1r"]);
    assert_eq!((res.cut_line, res.end_line), (2, 4));
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (1, 0));
    assert_eq!(res.omitted_lines, 2, "the malformed line and the turn after the close");

    let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("end"))).unwrap_err();
    assert!(
        err.to_string().contains("line 3 is not a complete JSON record"),
        "{err}"
    );
    cleanup(&dir);
}

/// The same, with the call never closed: the scan reaches EOF, the open call is reported, and a
/// live session's half-written last line (no newline) is omitted rather than refused.
#[test]
fn an_explicit_cut_left_open_omits_a_half_written_last_line() {
    let dir = tmpdir();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec("assistant", "a0", Some("u0"), bash("t1", "cat notes")),
        ],
    );
    let cut = idx_of(&src, "a0");
    append(&src, b"{\"type\":\"user\"");
    let before = std::fs::read(&src).unwrap();

    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &dir.join("out"))).unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0"]);
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (0, 1));
    assert_eq!(res.omitted_lines, 1);
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");
    cleanup(&dir);
}

/// Under `End` the last line of the captured prefix is copied, so a half-written one refuses; with
/// no newline the session may still be appending it, and the refusal says to rerun before it says
/// to repair. The same bytes WITH a newline are a finished bad record: no rerun is offered.
#[test]
fn end_names_a_rerun_for_an_unterminated_last_line_only() {
    for (tail, rerun) in [
        ("{\"type\":\"cost-state\"", true),
        ("{\"type\":\"cost-state\"\n", false),
    ] {
        let dir = tmpdir();
        let src = session_tailed(&dir);
        append(&src, tail.as_bytes());
        let out = dir.join("out");
        let msg = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("line 6"), "{msg}");
        assert_eq!(
            msg.contains("unterminated") && msg.contains("rerun the rewind"),
            rerun,
            "{msg}"
        );
        assert_eq!(msg.contains("not a complete JSON record"), !rerun, "{msg}");
        assert!(std::fs::read_dir(&out).unwrap().next().is_none(), "nothing published");
        cleanup(&dir);
    }
}

#[test]
fn present_message_body_variants_remain_parser_compatible() {
    for body in [
        json!(null),
        json!(false),
        json!(7),
        json!("bad"),
        json!([]),
        json!({}),
        json!({"content": null}),
        json!({"content": 7}),
        json!({"content": {}}),
    ] {
        for ty in ["user", "assistant"] {
            let dir = tmpdir();
            let src = session_tailed(&dir);
            let bad = rec(ty, "invalid-body", Some("a0"), body.clone());
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all((bad.to_string() + "\n").as_bytes())
                .unwrap();
            let before = std::fs::read(&src).unwrap();
            let out = dir.join("out");
            let parsed = crate::harness::claude::parse_str("synthetic", std::str::from_utf8(&before).unwrap(), None);
            assert_eq!(parsed.messages.len(), 3, "present body produces a message: {body}");
            let result = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap();
            assert_eq!(read_lines(&result.new_path)[5]["message"], body);
            assert_eq!((result.lines_written, result.omitted_lines), (6, 0));
            assert_eq!(std::fs::read(&src).unwrap(), before);
            cleanup(&dir);
        }
    }
}

#[test]
fn valid_empty_content_and_unknown_tail_survive_all_copy_paths() {
    for (thinking, skills_policy) in [
        (crate::emit::ThinkingMode::Native, SkillsPolicy::Keep),
        (crate::emit::ThinkingMode::Native, SkillsPolicy::DropAll),
        (crate::emit::ThinkingMode::Portable, SkillsPolicy::Keep),
    ] {
        let dir = tmpdir();
        let src = session_attachment_and_bad_tail(&dir, false);
        let extra = [
            rec("user", "empty", Some("a0"), json!({"role":"user", "content":[]})),
            json!({"type":"future-state", "sessionId":SID, "payload":{"untouched":true}}),
        ];
        for v in extra {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all((v.to_string() + "\n").as_bytes())
                .unwrap();
        }
        let options = RewindOptions {
            thinking: Some(thinking),
            skills_policy,
            ..opts(CutAt::End, &dir.join("out"))
        };
        let res = rewind_session(&sref(&src, SID), &options).unwrap();
        let got = read_lines(&res.new_path);
        assert_eq!(got.len(), 5);
        assert_eq!(got[4]["payload"]["untouched"], true);
        assert_eq!(got[4]["sessionId"], res.new_id);
        assert_eq!((res.end_line, res.omitted_lines), (5, 0));
        assert_eq!(
            got[2]["attachment"]["skills"].as_array().unwrap().len(),
            usize::from(skills_policy == SkillsPolicy::Keep)
        );
        cleanup(&dir);
    }
}

#[test]
fn full_changes_start_but_end_still_includes_compacted_tail() {
    for full in [false, true] {
        let dir = tmpdir();
        let src = session(&dir);
        for tail in tail_records() {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all((tail.to_string() + "\n").as_bytes())
                .unwrap();
        }
        let res = rewind_session(
            &sref(&src, SID),
            &RewindOptions {
                full,
                ..opts(CutAt::End, &dir.join("out"))
            },
        )
        .unwrap();
        assert_eq!(
            (res.start_line, res.end_line, res.omitted_lines),
            (if full { 1 } else { 8 }, 20, 0)
        );
        assert_eq!(read_lines(&res.new_path).last().unwrap()["type"], "cost-state");
        cleanup(&dir);
    }
}

#[test]
fn empty_and_metadata_only_sources_refuse_without_publication() {
    for body in ["", "\n", "{\"type\":\"cost-state\",\"sessionId\":\"synthetic\"}\n"] {
        let dir = tmpdir();
        let src = dir.join(format!("{SID}.jsonl"));
        std::fs::write(&src, body).unwrap();
        let out = dir.join("out");
        let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap_err();
        assert!(err.to_string().contains("no messages"), "{err}");
        assert!(!out.exists());
        assert_eq!(std::fs::read_to_string(src).unwrap(), body);
        cleanup(&dir);
    }
}

#[test]
fn a_cut_between_parallel_results_runs_on_until_the_calls_close() {
    let dir = tmpdir();
    let src = session(&dir);
    let res = rewind_session(
        &sref(&src, SID),
        &opts(CutAt::Message(idx_of(&src, "t1r")), &dir.join("o")),
    )
    .unwrap();
    // t2 was issued before the cut; its result (line 15, after a hook record) comes along, the
    // next assistant turn does not.
    assert_eq!((res.cut_line, res.end_line), (13, 15));
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (1, 0));
    let got = read_lines(&res.new_path);
    assert_eq!(uuids(&got).last(), Some(&"t2r"));
    assert!(res.warnings.is_empty(), "{:?}", res.warnings);
    cleanup(&dir);
}

#[test]
fn provenance_sidecar_records_source_cut_and_start_and_the_source_is_untouched() {
    let dir = tmpdir();
    let src = session(&dir);
    let before = std::fs::read(&src).unwrap();
    let out = dir.join("out");
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Commit(SHA[..12].into()), &out)).unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");

    assert_eq!(
        res.provenance_path,
        out.join("22222222-2222-4222-8222-222222222222.rewind.json")
    );
    let p: Value = serde_json::from_str(&std::fs::read_to_string(&res.provenance_path).unwrap()).unwrap();
    assert_eq!(p["format"], "cv-rewind");
    assert_eq!(p["source"]["id"], SID);
    assert_eq!(p["source"]["path"], src.display().to_string());
    assert_eq!(p["source"]["bytes"], before.len() as u64);
    assert_eq!(p["source"]["sha256"], crate::digest::sha256_hex(&before));
    assert_eq!(p["source"]["subagent"], false);
    assert_eq!(p["cut"]["by"], "commit");
    assert_eq!(p["cut"]["msg_idx"], idx_of(&src, "r1"));
    assert_eq!(p["cut"]["line"], 7);
    assert_eq!(p["cut"]["evidence"]["kind"], "created");
    assert_eq!(p["start"]["mode"], "from_compaction");
    assert_eq!(p["start"]["msg_idx"], idx_of(&src, "b1"));
    assert_eq!(p["start"]["line"], 4);
    assert_eq!(p["omitted_tail"]["lines"], 10);
    assert!(p["omitted_tail"]["note"].as_str().unwrap().contains("10 more line(s)"));
    assert_eq!(p["cv_version"], "cv-test");
    assert!(p["derived_at"].as_str().unwrap().starts_with("20"));

    // A dry run computes the same window and writes nothing.
    let dry = dir.join("dry");
    let d = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            dry_run: true,
            ..opts(CutAt::Commit(SHA.into()), &dry)
        },
    )
    .unwrap();
    assert_eq!((d.start_line, d.cut_line), (4, 7));
    assert!(!dry.exists());
    // An existing target is refused, never overwritten.
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    cleanup(&dir);
}

#[test]
fn subagent_transcript_becomes_a_standalone_resumable_session() {
    let dir = tmpdir();
    let parent = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &parent,
        &[rec("user", "p0", None, json!({"role": "user", "content": "spawn"}))],
    );
    let agent = dir.join(SID).join("subagents").join("agent-abc123.jsonl");
    let side = |ty: &str, uuid: &str, parent: Option<&str>, message: Value| {
        let mut v = rec(ty, uuid, parent, message);
        v["isSidechain"] = json!(true);
        v["agentId"] = json!("abc123");
        v["cwd"] = json!("/work/proj/wt"); // the sub-agent works in a worktree…
        v
    };
    write_jsonl(
        &agent,
        &[
            side("user", "x0", None, json!({"role": "user", "content": "fix the gate"})),
            side(
                "assistant",
                "x1",
                Some("x0"),
                bash("toolu_x", "git commit -qam fix && git rev-parse HEAD"),
            ),
            side("user", "x2", Some("x1"), result("toolu_x", SHA)),
            side("assistant", "x3", Some("x2"), text("assistant", "committed")),
        ],
    );
    let before = std::fs::read(&agent).unwrap();
    let r = sref(&agent, "agent-abc123");
    assert!(is_subagent(&r));
    // Default out = the PARENT's project dir (where `claude --resume` looks), not `subagents/`.
    assert_eq!(project_dir(&agent), dir);
    let res = rewind_session(
        &r,
        &RewindOptions {
            out_dir: None,
            ..opts(CutAt::End, &dir)
        },
    )
    .unwrap();
    assert!(res.subagent);
    assert_eq!(res.new_path.parent().unwrap(), dir);
    assert_eq!(res.source_session_id.as_deref(), Some(SID));
    let got = read_lines(&res.new_path);
    assert!(got
        .iter()
        .all(|v| v["isSidechain"] == false && v.get("agentId").is_none()));
    assert!(got
        .iter()
        .all(|v| v["sessionId"] == "22222222-2222-4222-8222-222222222222"));
    // The whole transcript is one root chain the loader can walk to a null parent.
    assert_eq!(chain(&got), ["x3", "x2", "x1", "x0"]);
    // …but resumes from the parent's launch dir, the one Claude files `-work-proj` under.
    assert_eq!(res.cwd.as_deref(), Some("/work/proj"));
    assert_eq!(res.cut_cwd.as_deref(), Some("/work/proj/wt"));
    assert!(res.warnings.is_empty(), "{:?}", res.warnings);
    // And it reads back as an ordinary (non-sub-agent) session.
    let s = crate::harness::claude::parse_str("n", &std::fs::read_to_string(&res.new_path).unwrap(), None);
    assert_eq!(s.messages.len(), 4);
    assert_eq!(std::fs::read(&agent).unwrap(), before);

    // A sub-agent's commit is exact evidence too (a full `rev-parse` sha names a short query).
    let short = rewind_session(
        &r,
        &RewindOptions {
            dry_run: true,
            ..opts(CutAt::Commit(SHA[..7].into()), &dir)
        },
    )
    .unwrap();
    assert_eq!(short.cut_line, 3);
    cleanup(&dir);
}

#[test]
fn commit_evidence_reports_only_git_proof_of_the_queried_shas() {
    let dir = tmpdir();
    let src = session(&dir);
    let r = sref(&src, SID);
    let ev = commit_evidence(&r, &[SHA.to_string(), "0123456789abcdef".into()]).unwrap();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!((ev[0].kind, ev[0].msg_idx), (EvidenceKind::Created, idx_of(&src, "r1")));
    assert!(ev[0].byte_offset.is_none(), "plain lazy() streams carry no offsets");
    assert!(commit_evidence(&r, &["fedcba9876".into()]).unwrap().is_empty());
    assert!(commit_evidence(&r, &["not-a-sha".into()]).unwrap().is_empty());
    cleanup(&dir);
}

#[test]
fn file_mentions_finds_needles_across_chunk_boundaries() {
    let dir = tmpdir();
    let p = dir.join("big.jsonl");
    // Straddle the 4 MiB read boundary with one needle; put the other at the very end.
    let mut body = vec![b'x'; (4 << 20) - 3];
    body.extend_from_slice(b"ABCDEF1");
    body.extend_from_slice(&vec![b'y'; 1000]);
    body.extend_from_slice(b"9876543");
    std::fs::write(&p, &body).unwrap();
    assert_eq!(
        file_mentions(&p, &["abcdef1", "9876543", "1111111"]).unwrap(),
        [true, true, false]
    );
    cleanup(&dir);
}

#[test]
fn sha_tokens_match_whole_hex_runs_prefix_compatibly() {
    let sha = normalize_sha(SHA).unwrap();
    assert!(output_names_sha("[main 63cef47] gate fix", &sha));
    assert!(output_names_sha("   1a2b3c4..63cef473b77  HEAD -> main", &sha));
    assert!(output_names_sha(SHA, &normalize_sha("63CEF47").unwrap()));
    // Inside a longer hex run that merely CONTAINS the sha's digits: not a match.
    assert!(!output_names_sha("ff63cef473b771", &sha));
    // Too short to mean anything.
    assert!(!output_names_sha("[main 63cef4] x", &sha));
    assert!(!output_names_sha("[main 63cef48] x", &sha));
    assert_eq!(normalize_sha(" 63CEF47 "), Some("63cef47".into()));
    assert_eq!(normalize_sha("63cef4"), None);
    assert_eq!(normalize_sha("63cef4z9"), None);
}

#[test]
fn git_verbs_classify_created_and_pushed() {
    use EvidenceKind::*;
    assert_eq!(git_evidence_kind("git commit -m x"), Some(Created));
    assert_eq!(
        git_evidence_kind("cd /r && git -C /r -c user.name=a commit -qm 'x'"),
        Some(Created)
    );
    assert_eq!(git_evidence_kind("git --no-pager merge feature"), Some(Created));
    assert_eq!(git_evidence_kind("git cherry-pick abc1234"), Some(Created));
    assert_eq!(git_evidence_kind("git push -u origin HEAD"), Some(Pushed));
    // A push after a commit in one command is still a creation.
    assert_eq!(git_evidence_kind("git push; git commit -am y"), Some(Created));
    assert_eq!(git_evidence_kind("git log --oneline"), None);
    assert_eq!(git_evidence_kind("git commit-tree abc"), None);
    assert_eq!(git_evidence_kind("echo 'legit commit'"), None);
    assert_eq!(git_evidence_kind("/usr/bin/git commit -m z"), Some(Created));
}

#[test]
fn rewind_prefix_receipt_accepts_appends_and_rejects_source_mutations() {
    for mutation in ["append", "rewrite", "replace", "rewrite-grow", "truncate"] {
        let dir = tmpdir();
        let path = dir.join(format!("{SID}.jsonl"));
        let initial = rec("user", "u", None, text("user", "original prefix"));
        write_jsonl(&path, &[initial]);
        let original = std::fs::read(&path).unwrap();
        let r = SessionRef::from_path(&path, Harness::Claude);
        let opts = RewindOptions {
            new_id: Some("derived".into()),
            out_dir: Some(dir.clone()),
            ..opts(CutAt::End, &dir)
        };
        let result = rewind_session_after_scan(&r, &opts, || {
            match mutation {
                "append" => {
                    let mut writer = std::fs::OpenOptions::new().append(true).open(&path)?;
                    writer.write_all(b"\n")?;
                }
                "replace" => {
                    let replacement = dir.join("replacement.jsonl");
                    std::fs::write(&replacement, &original)?;
                    std::fs::rename(replacement, &path)?;
                }
                "truncate" => {
                    std::fs::write(&path, b"")?;
                }
                other => {
                    let mut changed = original.clone();
                    let start = changed
                        .windows(b"original".len())
                        .position(|w| w == b"original")
                        .unwrap();
                    changed[start..start + 8].copy_from_slice(b"modified");
                    if other == "rewrite-grow" {
                        changed.extend_from_slice(b"\n");
                    }
                    std::fs::write(&path, changed)?;
                }
            }
            Ok(())
        });
        if mutation == "append" {
            let result = result.unwrap();
            assert_eq!(result.source_bytes, original.len() as u64);
            assert_eq!(result.source_sha256, {
                let mut hash = crate::digest::Sha256::new();
                hash.update(&original);
                hash.finish_hex()
            });
            let actual = std::fs::read_to_string(result.new_path).unwrap();
            assert_eq!(
                actual,
                crate::restamp::restamp(std::str::from_utf8(&original).unwrap(), "derived").body
            );
        } else {
            assert!(result.is_err(), "{mutation} must not derive inconsistent output");
            assert!(!dir.join("derived.jsonl").exists());
            assert!(!dir.join("derived.rewind.json").exists());
        }
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cv-rewind-")));
        cleanup(&dir);
    }
}

#[test]
fn rewind_rejects_output_ids_that_escape_the_output_directory() {
    let dir = tmpdir();
    let source = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&source, &[rec("user", "u", None, text("user", "original"))]);
    let reference = SessionRef::from_path(&source, Harness::Claude);
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
        let error = rewind_session(
            &reference,
            &RewindOptions {
                new_id: Some(id.into()),
                ..opts(CutAt::End, &dir)
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("filename component"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    cleanup(&dir);
}
