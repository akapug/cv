use super::*;
use crate::ir::{Block, Message, MessageKind, Role, Usage};
use serde_json::json;

fn text(role: Role, kind: MessageKind, id: &str, t: &str) -> Message {
    let mut m = Message::new(role);
    m.kind = kind;
    m.id = Some(id.into());
    m.content.push(Block::Text {
        text: t.to_string().into(),
    });
    m
}

fn reply(id: &str, t: &str, tokens: u64) -> Message {
    let mut m = text(Role::Assistant, MessageKind::Reply, id, t);
    m.usage = Some(Usage {
        input_tokens: Some(tokens),
        ..Default::default()
    });
    m
}

fn tool_result(id: &str) -> Message {
    let mut m = Message::new(Role::Tool);
    m.kind = MessageKind::ToolResult;
    m.id = Some(id.into());
    m.content.push(Block::ToolResult {
        tool_use_id: "t".into(),
        content: "output that the harness may truncate".to_string().into(),
        is_error: false,
        tool_name: None,
        status: None,
        details: None,
    });
    m
}

fn window(messages: &[Message]) -> Window {
    let resolver = crate::lazy::Resolver::new(None);
    let mut w = Window::default();
    for m in messages {
        w.observe(m, &resolver).unwrap();
    }
    w
}

fn record_at(w: &Window) -> Record {
    let (cut, anchors) = w.cut_now().unwrap();
    Record {
        v: RECORD_VERSION,
        harness: "claude".into(),
        session: "s".into(),
        window: WindowRef {
            boundaries: w.boundaries.len(),
            start: w.start,
        },
        cut,
        anchors,
        profile: profile_id("Summarize the conversation so far."),
        model: "m".into(),
        created: chrono::Utc::now(),
        base: None,
        bounds: Vec::new(),
        text: "<summary>S</summary>".into(),
    }
}

fn session() -> Vec<Message> {
    vec![
        text(
            Role::User,
            MessageKind::Prompt,
            "u1",
            "please refactor the parser module",
        ),
        reply("a1", "I will refactor the parser module now.", 1000),
        tool_result("r1"),
        reply("a2", "The parser is refactored and the tests pass.", 2000),
    ]
}

fn request(extra: serde_json::Value) -> serde_json::Value {
    let mut messages = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>context</system-reminder>"},
            {"type": "text", "text": "please refactor the parser module"}]}),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "I will refactor the parser module now."},
            {"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "trunc"}]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "The parser is refactored and the tests pass."}]}),
    ];
    if let serde_json::Value::Array(more) = extra {
        messages.extend(more);
    }
    messages.push(json!({"role": "user", "content": "Summarize the conversation so far."}));
    json!({"metadata": {"user_id": "{\"session_id\":\"s\"}"}, "messages": messages})
}

#[test]
fn a_current_record_matches_with_an_empty_tail() {
    let w = window(&session());
    let record = record_at(&w);
    assert!(record.cut.clean);
    let view = RequestView::parse(&request(json!([]))).unwrap();
    let m = validate(&view, &record).unwrap();
    assert_eq!(m.anchors, 3);
    assert!(view.tail(m.tail_from).is_empty(), "{:?}", view.tail(m.tail_from));
}

#[test]
fn messages_after_the_cut_are_the_tail() {
    let record = record_at(&window(&session()));
    let view = RequestView::parse(&request(json!([
        {"role": "user", "content": "now update the writer as well"},
        {"role": "assistant", "content": "Updating the writer."}
    ])))
    .unwrap();
    let m = validate(&view, &record).unwrap();
    let tail = view.tail(m.tail_from);
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].texts, vec!["now update the writer as well"]);
}

#[test]
fn a_changed_history_or_instruction_misses() {
    let record = record_at(&window(&session()));
    let mut body = request(json!([]));
    body["messages"][3]["content"][0]["text"] = json!("A different answer entirely, after a rewind.");
    let view = RequestView::parse(&body).unwrap();
    assert_eq!(validate(&view, &record), Err(Miss::Anchor { index: 2, of: 3 }));

    let mut body = request(json!([]));
    let last = body["messages"].as_array().unwrap().len() - 1;
    body["messages"][last]["content"] = json!("Summarize differently.");
    let view = RequestView::parse(&body).unwrap();
    assert!(matches!(validate(&view, &record), Err(Miss::Profile { .. })));
}

#[test]
fn a_cut_followed_by_tool_work_keeps_the_last_anchor_message_in_the_tail() {
    let mut msgs = session();
    msgs.truncate(3); // the cut is the tool result after the second anchor
    let record = record_at(&window(&msgs));
    assert!(!record.cut.clean);
    let view = RequestView::parse(&request(json!([]))).unwrap();
    let m = validate(&view, &record).unwrap();
    // The message holding the last matched anchor (the assistant's first reply) starts the tail.
    assert_eq!(m.tail_from, 1);
}

#[test]
fn a_compaction_starts_a_new_window_and_native_precompute_is_seen() {
    let mut msgs = session();
    let mut boundary = Message::new(Role::System);
    boundary.kind = MessageKind::CompactionBoundary;
    boundary.extra.insert(
        "claude".into(),
        json!({"compactMetadata": {"trigger": "auto", "preTokens": 200_000, "durationMs": 120, "precomputed": true}}),
    );
    msgs.push(boundary);
    msgs.push(text(
        Role::User,
        MessageKind::CompactionSummary,
        "cs",
        "<summary>earlier work summarized</summary>",
    ));
    let w = window(&msgs);
    assert_eq!(w.start, 5);
    assert_eq!(w.anchors.len(), 0, "a summary that is all markup anchors nothing");
    assert!(w.native_precompute());
    assert_eq!(w.compact_point(), Some(200_000));
    assert_eq!(
        due(&Settings::default(), &w, Some(10), None, 0),
        Due::Wait("the harness precomputes its own compactions")
    );
}

#[test]
fn due_starts_at_the_fraction_and_extends_by_tokens_or_idle() {
    let s = Settings::default();
    let w = window(&session()); // fill 2000
    assert_eq!(
        due(&s, &w, None, None, 0),
        Due::Wait("auto-compact point unknown (set precompact.compact_at)")
    );
    assert_eq!(
        due(&s, &w, Some(10_000), None, 0),
        Due::Wait("below the start threshold")
    );
    assert_eq!(due(&s, &w, Some(2_500), None, 0), Due::Start);
    let mut cut = w.cut_now().unwrap().0;
    assert_eq!(
        due(&s, &w, Some(2_500), Some(&cut), 999),
        Due::Wait("the summary is current")
    );
    cut.index = 1;
    cut.tokens = Some(1000);
    assert_eq!(
        due(&s, &w, Some(2_500), Some(&cut), 0),
        Due::Wait("waiting for more tokens or for the session to go idle")
    );
    assert_eq!(due(&s, &w, Some(2_500), Some(&cut), s.idle_secs), Due::Extend);
}

fn call(m: &mut Message, id: &str) {
    m.content.push(Block::ToolUse {
        id: id.into(),
        name: "Bash".into(),
        input: json!({}).into(),
        namespace: None,
    });
}

#[test]
fn tool_call_ids_pin_a_cut_inside_tool_work() {
    let mut a1 = reply("a1", "I will refactor the parser module now.", 1000);
    call(&mut a1, "toolu_A");
    let mut a2 = Message::new(Role::Assistant);
    a2.kind = MessageKind::Reply;
    a2.id = Some("a2".into());
    call(&mut a2, "toolu_B");
    let msgs = vec![
        text(
            Role::User,
            MessageKind::Prompt,
            "u1",
            "please refactor the parser module",
        ),
        a1,
        tool_result("r1"),
        a2,
        tool_result("r2"),
    ];
    let record = record_at(&window(&msgs));
    assert_eq!(record.anchors.len(), 4, "two prose segments and two call ids");
    assert!(!record.cut.clean);
    let body = |b: &str| {
        json!({"messages": [
            {"role": "user", "content": "please refactor the parser module"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "I will refactor the parser module now."},
                {"type": "tool_use", "id": "toolu_A", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_A", "content": "x"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": b, "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": b, "content": "x"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_C", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_C", "content": "x"},
                {"type": "text", "text": "Summarize the conversation so far."}]}
        ]})
    };
    let view = RequestView::parse(&body("toolu_B")).unwrap();
    let m = validate(&view, &record).unwrap();
    // The tail starts at the last call the summary covered, not at the last prose before it.
    assert_eq!(m.tail_from, 3);
    assert_eq!(view.tail(m.tail_from).len(), 4);
    // A call id the summary covered is gone: the history changed.
    let view = RequestView::parse(&body("toolu_B2")).unwrap();
    assert_eq!(validate(&view, &record), Err(Miss::Anchor { index: 3, of: 4 }));
    // A longer id containing the covered one is not it.
    let view = RequestView::parse(&body("toolu_BB")).unwrap();
    assert!(validate(&view, &record).is_err());
}

#[test]
fn a_turn_that_drew_an_error_leaves_the_fingerprint() {
    let mut error = Message::new(Role::System);
    error.kind = MessageKind::Error;
    error.content.push(Block::Text {
        text: "You've hit your usage limit".to_string().into(),
    });
    let msgs = vec![
        text(
            Role::User,
            MessageKind::Prompt,
            "u1",
            "please refactor the parser module",
        ),
        reply("a1", "I will refactor the parser module now.", 1000),
        // Two queued prompts, one turn, and the turn drew an error.
        text(Role::User, MessageKind::Prompt, "u2", "the hourly check-in, run it now"),
        text(
            Role::User,
            MessageKind::Prompt,
            "u2b",
            "and the disk report while you are there",
        ),
        error,
        text(
            Role::User,
            MessageKind::InjectedContext,
            "n1",
            "<task-notification>done</task-notification>",
        ),
        text(
            Role::User,
            MessageKind::Prompt,
            "u3",
            "the parser tests, are they green",
        ),
        reply("a3", "The parser is refactored and the tests pass.", 2000),
    ];
    let record = record_at(&window(&msgs));
    assert_eq!(record.anchors.len(), 4, "the failed turn's anchors are gone");
    // The harness sent the history without the failed prompt; its text comes back later.
    let view = RequestView::parse(&json!({"messages": [
        {"role": "user", "content": "please refactor the parser module"},
        {"role": "assistant", "content": "I will refactor the parser module now."},
        {"role": "user", "content": "the parser tests, are they green"},
        {"role": "assistant", "content": "The parser is refactored and the tests pass."},
        {"role": "user", "content": "the hourly check-in, run it now"},
        {"role": "assistant", "content": "Done."},
        {"role": "user", "content": "Summarize the conversation so far."}
    ]}))
    .unwrap();
    let m = validate(&view, &record).unwrap();
    assert_eq!(m.tail_from, 4);
}
