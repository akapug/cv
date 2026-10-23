//! Protocol-level integration tests for `cv mcp`, the stdio JSON-RPC server: spawn the real binary,
//! run the MCP handshake, list tools, and call them against a temp-home fixture corpus.
//!
//! Hermetic: a fake `$HOME` carries the claude fixtures and `$CLUSTERVISION_HOME` the index/task
//! state; both are passed only to the child process's environment (and from it to the `cv` it
//! re-runs for a generated tool). The argv a generated call builds is pinned by `mcp_tools`' unit
//! tests; these check the round trip through the real commands.

use cv_testkit::World;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};

use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A running `cv mcp` server over stdio, with a line reader on a side thread so a hung server
/// fails the test with a timeout instead of wedging the run.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    world: World,
}

impl Server {
    fn spawn(tag: &str) -> Server {
        let world = World::new(tag);

        // Fixture corpus: two tiny claude sessions.
        let alpha = [
            json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done — the migration is fixed"}]}}),
        ];
        let beta = [
            json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "how many quokkas in the census"}}),
            json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "seventeen quokkas"}]}}),
        ];
        world.write_session("alphasess", &alpha);
        world.write_session("betasess", &beta);

        let mut child = world
            .cv_command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cv mcp should spawn");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Server {
            child,
            stdin,
            lines: rx,
            world,
        }
    }

    /// Send one raw line to the server.
    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to cv mcp stdin");
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, v: &Value) {
        self.send_raw(&v.to_string());
    }

    /// Next response line as JSON. The timeout exists to turn a WEDGE into a failure, not to gate
    /// on speed: 10s was tight enough that `observe_stream_bounded_read_only_tail` failed at
    /// exactly 10.018s under a loaded full-suite run while passing in 2.5s on its own, and a
    /// generated tool now shells out to the `cv` binary, which is not free. A minute still fails a
    /// genuine hang promptly and stops reporting the machine's load as a bug.
    fn recv(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("timed out waiting for a cv mcp response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON response {line:?}: {e}"))
    }

    /// Round-trip a request and assert the JSON-RPC envelope (version + echoed id).
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let resp = self.recv();
        assert_eq!(resp["jsonrpc"], "2.0", "envelope: {resp}");
        assert_eq!(resp["id"], json!(id), "id must echo: {resp}");
        resp
    }

    /// Call a tool and return `(text, is_error)` from the MCP tool result.
    fn call_tool(&mut self, id: u64, name: &str, args: Value) -> (String, bool) {
        let resp = self.request(id, "tools/call", json!({"name": name, "arguments": args}));
        let result = &resp["result"];
        assert!(result.is_object(), "tools/call must return a result: {resp}");
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("tool result must carry content: {resp}"));
        assert_eq!(content[0]["type"], "text", "{resp}");
        let text = content[0]["text"].as_str().unwrap_or_default().to_string();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (text, is_error)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_and_tool_schemas() {
    let mut s = Server::spawn("handshake");

    // initialize: protocol version, server info, capabilities.
    let resp = s.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let r = &resp["result"];
    assert!(r["protocolVersion"].is_string(), "{resp}");
    assert_eq!(r["serverInfo"]["name"], "clustervision", "{resp}");
    assert!(r["capabilities"]["tools"].is_object(), "{resp}");

    // The initialized notification must produce no response — the next reply must be for id 2.
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let resp = s.request(2, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");

    // tools/list: every tool is named, described, and carries a valid object schema whose
    // `required` names actually exist in `properties`.
    let resp = s.request(3, "tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert!(tools.len() >= 5, "expected the full toolset, got {}", tools.len());
    let mut names = std::collections::HashSet::new();
    for t in tools {
        let name = t["name"].as_str().unwrap_or_else(|| panic!("unnamed tool: {t}"));
        assert!(names.insert(name.to_string()), "duplicate tool name {name}");
        assert!(
            !t["description"].as_str().unwrap_or("").is_empty(),
            "{name} needs a description"
        );
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}: inputSchema must be type:object");
        let props = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name}: properties must be an object"));
        for (pname, p) in props {
            assert!(p["type"].is_string(), "{name}.{pname}: every property needs a type");
            assert!(
                !p["description"].as_str().unwrap_or("").is_empty(),
                "{name}.{pname}: every property needs a description"
            );
        }
        if let Some(req) = schema["required"].as_array() {
            for r in req {
                let r = r.as_str().expect("required entries are strings");
                assert!(props.contains_key(r), "{name}: required {r:?} missing from properties");
            }
        }
    }
    // Generated from the CLI's Read group…
    for expected in ["ls", "show", "cat", "search"] {
        assert!(
            names.contains(expected),
            "missing CLI-generated tool {expected}: {names:?}"
        );
    }
    // …the task subcommands, generated the same way…
    for expected in ["task_open", "task_claim", "task_resolve", "task_inbox", "task_events"] {
        assert!(names.contains(expected), "missing CLI-generated tool {expected}");
    }
    assert!(!names.contains("task"), "`task` is a dispatcher, not a tool: {names:?}");
    // …and the two hand-written tools, which block or hold a cursor.
    for expected in ["observe_stream", "await_omen"] {
        assert!(names.contains(expected), "missing MCP-only tool {expected}");
    }
    // `resolve` answers with one of choice / accept_default / confirm: none is required alone.
    let resolve = tools.iter().find(|t| t["name"] == "task_resolve").unwrap();
    assert_eq!(resolve["inputSchema"]["required"], json!(["id"]), "{resolve}");
    // Fleet & live commands (`serve`, `watch`, `mcp`) are never tools: they block or hold a socket.
    for gone in ["serve", "watch", "archive_sync", "mcp", "scry"] {
        assert!(!names.contains(gone), "{gone} must not be a generated tool: {names:?}");
    }
    // The 0.11.0 removals stay removed: their replacements are show/search/ls/pack/prune/cat.
    for gone in [
        "list_sessions",
        "search_sessions",
        "read_session",
        "project_sessions",
        "recall",
        "prune_session",
        "prune_retrieve",
    ] {
        assert!(
            !names.contains(gone),
            "{gone} was removed in 0.11.0 but is still advertised"
        );
    }

    // `show` says no window is the whole transcript, and `--json` is never the caller's to set.
    let show = tools.iter().find(|t| t["name"] == "show").unwrap();
    let desc = show["description"].as_str().unwrap();
    assert!(desc.contains("returns the whole transcript"), "{desc}");
    let props = &show["inputSchema"]["properties"];
    assert!(props.get("json").is_none(), "json must not be a property: {props}");
    assert_eq!(props["last"]["type"], "integer", "{props}");
    assert_eq!(show["inputSchema"]["required"], json!(["id"]), "{show}");
}

/// The generated tools run the real commands: `show` with no window returns the whole IR as JSON, `cat` (no JSON form) returns text, a non-zero exit is a tool error
/// carrying stderr, and a protocol mistake is -32602 before anything runs.
#[test]
fn generated_tools_run_the_real_commands() {
    let mut s = Server::spawn("generated");
    s.request(1, "initialize", json!({}));

    let (text, is_err) = s.call_tool(2, "show", json!({"id": "alphasess"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("show returns the IR as JSON");
    assert_eq!(v["id"], "alphasess", "{text}");
    assert_eq!(v["messages"].as_array().map(Vec::len), Some(2), "{text}");

    // An explicit window is passed through (a clap id becomes its kebab-case flag).
    let (text, is_err) = s.call_tool(3, "show", json!({"id": "alphasess", "first": 1}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["messages"].as_array().map(Vec::len), Some(1), "{text}");

    // `ls` filters through to the same rows `cv ls --json` prints.
    let (text, is_err) = s.call_tool(4, "ls", json!({"cwd": "/work/proj"}));
    assert!(!is_err, "{text}");
    let rows: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rows.as_array().map(Vec::len), Some(2), "{text}");

    // A non-zero exit becomes an isError tool result carrying stderr.
    let (text, is_err) = s.call_tool(5, "show", json!({"id": "nosuch"}));
    assert!(is_err, "a failing `cv` must be a tool error: {text}");
    assert!(text.contains("nosuch"), "stderr must reach the caller: {text}");

    // A missing required positional and an unknown argument are the caller's protocol mistakes.
    let resp = s.request(8, "tools/call", json!({"name": "cat", "arguments": {"session": "a"}}));
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    assert!(
        resp["error"]["message"].as_str().unwrap().contains("tool_use_id"),
        "{resp}"
    );
    let resp = s.request(
        9,
        "tools/call",
        json!({"name": "show", "arguments": {"id": "a", "limit": 5}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    assert!(resp["error"]["message"].as_str().unwrap().contains("limit"), "{resp}");
}

#[test]
fn tool_calls_against_fixture_corpus() {
    let mut s = Server::spawn("tools");
    s.request(1, "initialize", json!({}));

    // The board tools went away in 0.15.0: a pointer at the task log, never a silent unknown-tool.
    for name in ["board_post", "board_read", "board_await", "board_claim"] {
        let (text, is_err) = s.call_tool(12, name, json!({"channel": "testchan", "body": "hello fleet"}));
        assert!(is_err, "{name}: {text}");
        assert!(
            text.contains("removed in 0.15.0") && text.contains("task_note"),
            "{name} must point at the replacement: {text}"
        );
    }
    assert!(!s.world.cv_home.join("board").exists(), "nothing wrote a board dir");
}

#[test]
fn malformed_utf8_is_a_frame_error_instead_of_rewritten_arguments() {
    let mut server = Server::spawn("invalid-utf8-frame");
    server
        .stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"ping\",\"params\":{\"text\":\"\xff\"}}\n")
        .unwrap();
    server.stdin.flush().unwrap();
    let error = server.recv();
    assert_eq!(error["error"]["code"], -32700, "invalid input was rewritten: {error}");
    assert!(error["id"].is_null());
    assert_eq!(server.request(100, "ping", json!({}))["id"], 100);
}

#[test]
fn malformed_observation_strings_are_rejected_instead_of_widening_filters() {
    let mut s = Server::spawn("string-arguments");
    for (tool, args) in [
        ("observe_stream", json!({"harness":false})),
        ("observe_stream", json!({"harness":"not-a-harness"})),
        ("observe_stream", json!({"cwd_contains":42})),
        ("observe_stream", json!({"since_cursor":{}})),
        ("await_omen", json!({"regex":false})),
        ("await_omen", json!({"regex":"never", "cwd_contains":[]})),
        ("await_omen", json!({})),
    ] {
        let response = s.request(1, "tools/call", json!({"name":tool, "arguments":args}));
        assert_eq!(response["error"]["code"], -32602, "{tool}: {response}");
    }
    let (text, error) = s.call_tool(
        2,
        "observe_stream",
        json!({"harness":null, "cwd_contains":"", "since_cursor":null}),
    );
    assert!(!error, "{text}");
    let baseline: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(baseline["baseline"], true);
}

#[test]
fn await_omen_timeout_caps_the_poll_sleep() {
    let mut s = Server::spawn("poll-deadline");
    // This would sleep for an hour before the repair. The protocol reader's ordinary
    // response deadline verifies completion without a throughput/timing benchmark.
    let (text, is_error) = s.call_tool(
        1,
        "await_omen",
        json!({
            "regex":"a message that never arrives", "timeout_secs":1, "interval_secs":3600,
        }),
    );
    assert!(!is_error, "{text}");
    let result: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(result["matched"], false);
    assert_eq!(result["timed_out"], true);
    assert_eq!(s.request(2, "ping", json!({}))["id"], 2);
}

#[test]
fn invalid_envelopes_and_tool_parameters_do_not_execute() {
    let mut s = Server::spawn("invalid-envelopes");
    for request in [
        json!(null),
        json!([]),
        json!(42),
        json!({"jsonrpc":"2.0", "id":11}),
        json!({"jsonrpc":"2.0", "id":{}, "method":"ping"}),
        json!({"jsonrpc":"1.0", "id":12, "method":"tools/call", "params":{"name":"task_open", "arguments":{"title":"must not exist"}}}),
        json!({"jsonrpc":"2.0", "id":13, "method":false}),
        json!({"jsonrpc":"2.0", "id":14, "method":"ping", "params":false}),
    ] {
        s.send(&request);
        let response = s.recv();
        assert_eq!(response["error"]["code"], -32600, "request {request}: {response}");
    }
    for params in [
        json!([]),
        json!({}),
        json!({"name":false}),
        json!({"name":"task_open", "arguments":[]}),
    ] {
        let response = s.request(20, "tools/call", params);
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
    // Malformed notifications remain response-free; the next response belongs to this ping.
    s.send(&json!({"jsonrpc":"2.0", "method":"tools/call", "params":{"name":"task_open", "arguments":false}}));
    assert_eq!(s.request(21, "ping", json!({}))["id"], 21);
    let (tasks, _) = s.world.cv_ok(&["task", "list", "--json"]);
    assert!(!tasks.contains("must not exist"), "invalid request executed: {tasks}");
}

#[test]
fn protocol_error_handling() {
    let mut s = Server::spawn("errors");
    s.request(1, "initialize", json!({}));

    // Unknown method with an id → -32601.
    let resp = s.request(2, "wizardry/cast", json!({}));
    assert_eq!(resp["error"]["code"], -32601, "{resp}");

    // Unparseable line → -32700 with a null id (and the server survives).
    s.send_raw("this is not json {");
    let resp = s.recv();
    assert_eq!(resp["error"]["code"], -32700, "{resp}");
    assert!(resp["id"].is_null(), "{resp}");

    // Unknown tool → a tool-level error result, not a protocol error.
    let (text, is_err) = s.call_tool(3, "tools_that_do_not_exist", json!({}));
    assert!(is_err, "{text}");
    assert!(text.contains("unknown tool"), "{text}");

    // A *notification* (no id) must never get a response — even for a known method. The next
    // line on the wire after the notifications must be the reply to the request that follows.
    s.send(&json!({"jsonrpc": "2.0", "method": "ping"}));
    s.send(&json!({"jsonrpc": "2.0", "method": "tools/list"}));
    let resp = s.request(4, "ping", json!({}));
    assert_eq!(
        resp["id"],
        json!(4),
        "a notification (no id) must not produce a response; got an extra reply: {resp}"
    );

    // Still alive and well after all of that.
    let resp = s.request(5, "tools/list", json!({}));
    assert!(resp["result"]["tools"].is_array(), "{resp}");
}

/// Requests are dispatched concurrently: a parked long-poll (await_omen on a regex nothing
/// emits) must not head-of-line-block a fast request issued after it. Responses come back out
/// of order — the fast one first — and both complete well under the long-poll's timeout budget.
#[test]
fn slow_long_poll_does_not_block_other_requests() {
    let mut s = Server::spawn("concurrent");
    s.request(1, "initialize", json!({}));

    let start = std::time::Instant::now();
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "await_omen",
        "arguments": {"regex": "NEVER_MATCHES", "cwd_contains": "/nowhere", "timeout_secs": 6, "interval_secs": 1}}}));
    s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "task_list", "arguments": {}}}));

    let first = s.recv();
    let elapsed = start.elapsed();
    assert_eq!(first["id"], json!(3), "the fast request must answer first: {first}");
    assert!(
        elapsed < Duration::from_secs(4),
        "task_list took {elapsed:?} — blocked behind the 6s long-poll?"
    );
    assert_eq!(first["result"]["isError"], false, "{first}");

    // The long-poll still completes on its own schedule (timed out, unmatched).
    let second = s.recv();
    assert_eq!(second["id"], json!(2), "{second}");
    let text = second["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("\"timed_out\": true"), "{second}");
}

/// A present-but-malformed numeric argument is the caller's protocol mistake: JSON-RPC `-32602
/// Invalid params`, not a silent fallback to the default (and not an isError tool result). An
/// integral float is tolerated — clients often serialize `1` as `1.0`.
#[test]
fn malformed_numeric_args_are_invalid_params() {
    let mut s = Server::spawn("badparams");
    s.request(1, "initialize", json!({}));

    for (id, bad) in [(2u64, json!("ten")), (3, json!(-3)), (4, json!(1.5))] {
        let resp = s.request(
            id,
            "tools/call",
            json!({"name": "observe_stream", "arguments": {"cwd_contains": "/nowhere", "max_messages": bad}}),
        );
        assert_eq!(resp["error"]["code"], -32602, "max_messages={bad}: {resp}");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("max_messages"),
            "the error must name the offending argument: {resp}"
        );
        assert!(resp["result"].is_null(), "an error response carries no result: {resp}");
    }

    // Integral float: accepted, behaves like the integer — on the hand-written guard…
    let (text, is_err) = s.call_tool(
        5,
        "observe_stream",
        json!({"cwd_contains": "/nowhere", "max_messages": 1.0}),
    );
    assert!(!is_err, "{text}");
    // …and on the generated path, which renders it for clap without the `.0`.
    let (text, is_err) = s.call_tool(9, "show", json!({"id": "alphasess", "last": 20.0}));
    assert!(!is_err, "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_ok(), "{text}");

    // Other tools route through the same guard (spot-check a long-poll's timeout).
    let resp = s.request(
        6,
        "tools/call",
        json!({"name": "await_omen", "arguments": {"regex": ".", "timeout_secs": "soon"}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");

    // Still alive afterwards.
    let resp = s.request(7, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");
}

/// `observe_stream` is the non-blocking tail of `await_omen`: bounded, read-only, cursor-driven.
/// Covers the spec's required cases — an absent/empty corpus yields a bounded EMPTY result, the
/// baseline call emits no backlog, the cursor drains only newly-appended messages, max_messages
/// bounds the batch (more_pending), and nothing is written anywhere.
#[test]
fn observe_stream_bounded_read_only_tail() {
    let mut s = Server::spawn("observe");
    s.request(1, "initialize", json!({}));

    // 1) Empty corpus (a filter that matches NO session) → a bounded, empty, baseline result.
    //    No crash, no error, an empty `messages`, and a usable cursor.
    let (text, is_err) = s.call_tool(2, "observe_stream", json!({"cwd_contains": "/no/such/dir"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("observe_stream returns JSON");
    assert_eq!(v["baseline"], true, "first call is a baseline: {text}");
    assert_eq!(v["count"], 0, "absent corpus → zero messages: {text}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{text}");
    assert_eq!(v["more_pending"], false, "{text}");
    // The cursor's encoding is opaque; its contract is that it resumes: replayed, the call is no
    // longer a baseline and still drains nothing.
    let empty = v["cursor"].as_str().expect("a cursor").to_string();
    let (text, is_err) = s.call_tool(
        102,
        "observe_stream",
        json!({"cwd_contains": "/no/such/dir", "since_cursor": empty}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], false, "the empty-corpus cursor resumes: {text}");
    assert_eq!(v["count"], 0, "{text}");

    // 2) Baseline over the REAL fixture corpus emits no backlog (only future activity is tailed),
    //    and hands back a cursor recording the current tail.
    let (text, is_err) = s.call_tool(3, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], true, "{text}");
    assert_eq!(v["count"], 0, "baseline emits no history: {text}");
    let cursor = v["cursor"].as_str().unwrap().to_string();

    // 3) Replaying that cursor with no new activity drains nothing (and is no longer a baseline).
    let (text, is_err) = s.call_tool(
        4,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], false, "a cursor call is not a baseline: {text}");
    assert_eq!(v["count"], 0, "no new activity → nothing drained: {text}");

    // 4) Append a NEW message to the alpha session on disk; the next cursor call drains exactly it.
    let proj = s.world.proj.clone();
    let extra = json!({"type": "assistant", "uuid": "a2",
                       "timestamp": "2026-06-19T12:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": "ZEBRA_TAIL_MARKER appended later"}]}});
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    body.push_str(&format!("{extra}\n"));
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();

    let (text, is_err) = s.call_tool(
        5,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    assert!(
        msgs.iter()
            .any(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")),
        "the appended message must be drained: {text}"
    );
    assert_eq!(
        v["count"], 1,
        "exactly the one appended message, nothing from before: {text}"
    );
    assert!(
        msgs.iter()
            .all(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")
                || !m["text"].as_str().unwrap_or("").contains("zebrafish")),
        "previously-seen messages must NOT re-emit: {text}"
    );

    // 5) max_messages bounds the batch. Append several messages, baseline-reset, then drain with a
    //    cap of 1 and assert more_pending is flagged.
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("burst{i}"),
                       "timestamp": "2026-06-19T13:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("burst message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), &body).unwrap();
    // Fresh baseline so we know exactly what's pending, then append AFTER baselining.
    let (text, _) = s.call_tool(6, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    let base2 = serde_json::from_str::<Value>(&text).unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("after{i}"),
                       "timestamp": "2026-06-19T14:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("after message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();
    let (text, is_err) = s.call_tool(
        7,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": base2, "max_messages": 1}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["count"], 1, "max_messages=1 bounds the batch: {text}");
    assert_eq!(v["more_pending"], true, "the rest must be flagged pending: {text}");

    // ...and polling again with the returned cursor drains the rest, one per call, in order —
    // with nothing new appended in between.
    let mut cursor = v["cursor"].as_str().unwrap().to_string();
    let mut drained = vec![v["messages"][0]["text"].as_str().unwrap().trim().to_string()];
    for id in 100..110 {
        let (text, is_err) = s.call_tool(
            id,
            "observe_stream",
            json!({"cwd_contains": "/work/proj", "since_cursor": cursor, "max_messages": 1}),
        );
        assert!(!is_err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        cursor = v["cursor"].as_str().unwrap().to_string();
        drained.extend(
            v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["text"].as_str().unwrap().trim().to_string()),
        );
        if v["more_pending"] == false {
            break;
        }
    }
    assert_eq!(
        drained,
        ["after message 0", "after message 1", "after message 2"],
        "more_pending must be drainable by polling the cursor"
    );

    // 6) READ-ONLY: observe_stream writes nothing under $CLUSTERVISION_HOME but the index.
    assert!(
        !s.world.cv_home.join("tasks").exists(),
        "observe_stream must not write the task log"
    );
}

/// A `since_cursor` this build cannot use (garbage, or another schema version) is a fresh
/// baseline, as `observe_stream`'s doc says: it records where every session stands and emits
/// nothing. Treating it as "no prior positions" instead makes every matching session look new,
/// and the call dumps the whole history of each one.
#[test]
fn observe_stream_unusable_cursor_is_a_fresh_baseline() {
    let mut s = Server::spawn("observe-badcursor");
    s.request(1, "initialize", json!({}));
    let mut id = 2;
    for bad in ["not a cursor", r#"{"v":99,"o":{}}"#, r#"{"v":1}"#] {
        let (text, is_err) = s.call_tool(
            id,
            "observe_stream",
            json!({"cwd_contains": "/work/proj", "since_cursor": bad}),
        );
        id += 1;
        assert!(!is_err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["count"], 0,
            "an unusable cursor must not dump history ({bad}): {text}"
        );
        assert_eq!(v["baseline"], true, "an unusable cursor is a baseline ({bad}): {text}");
        // ...and the cursor it hands back follows from there: nothing new, nothing drained.
        let cursor = v["cursor"].as_str().unwrap().to_string();
        let (text, _) = s.call_tool(
            id,
            "observe_stream",
            json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
        );
        id += 1;
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["count"], 0, "{text}");
    }
}

#[test]
fn admission_control_burst_preserves_successful_ids() {
    let mut server = Server::spawn("mcp-admission-controls");
    for id in 0..128 {
        server.send(&json!({"jsonrpc":"2.0", "id":id, "method":"tools/list"}));
    }
    let mut seen = std::collections::HashSet::new();
    for _ in 0..128 {
        let response = server.recv();
        assert!(response["result"]["tools"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(seen.insert(response["id"].as_u64().unwrap()));
        assert!(response.get("error").is_none());
    }
    assert_eq!(seen, (0..128).collect());
}

#[test]
fn admission_poll_saturation_keeps_ordinary_calls_responsive() {
    let mut server = Server::spawn("mcp-admission-polls");
    for id in 1..=3 {
        server.send(&json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
            "name":"await_omen", "arguments":{"regex":"NEVER_MATCHES", "cwd_contains":"/nowhere", "timeout_secs":4,"interval_secs":1}}}));
    }
    let busy = server.recv();
    assert_eq!(busy["id"], 3);
    assert_eq!(busy["error"]["code"], -32000);
    server.send(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"task_list","arguments":{}}}));
    let ordinary = server.recv();
    assert_eq!(ordinary["id"], 4);
    assert_eq!(ordinary["result"]["isError"], false);
    let mut polls = [
        server.recv()["id"].as_u64().unwrap(),
        server.recv()["id"].as_u64().unwrap(),
    ];
    polls.sort();
    assert_eq!(polls, [1, 2]);
}

#[test]
fn admission_shutdown_after_eof_or_output_failure_cancels_long_poll() {
    for output_failure in [false, true] {
        let world = World::new("mcp-admission-shutdown");
        let mut child = world
            .cv_command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"await_omen","arguments":{"regex":"never","timeout_secs":120}}})).unwrap();
        if output_failure {
            drop(child.stdout.take());
            writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":2,"method":"ping"})).unwrap();
        }
        drop(stdin);
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if start.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("MCP shutdown wedged");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn cancellation_targets_one_typed_request_and_preserves_connection() {
    let mut s = Server::spawn("request-cancellation");
    s.request(100, "initialize", json!({}));
    for id in [json!(1), json!("1")] {
        s.send(&json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
            "name":"await_omen", "arguments":{"regex":"NEVER_MATCHES", "cwd_contains":"/nowhere",
            "timeout_secs":120, "interval_secs":3600}}}));
    }
    s.send(&json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":1}}));
    let cancelled = s.recv();
    assert_eq!(cancelled["id"], json!(1), "{cancelled}");
    assert_eq!(cancelled["result"]["isError"], true, "{cancelled}");
    assert!(cancelled["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("MCP request cancelled"));

    // The numeric ID can immediately be reused, while the distinct string ID stays active.
    s.send(&json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
        "name":"task_list", "arguments":{}}}));
    let completed = s.recv();
    assert_eq!(completed["id"], json!(1), "{completed}");
    assert_eq!(completed["result"]["isError"], false, "{completed}");
    let ping = s.request(101, "ping", json!({}));
    assert_eq!(ping["result"], json!({}), "{ping}");
    s.send(&json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":"1"}}));
    let cancelled = s.recv();
    assert_eq!(cancelled["id"], json!("1"), "{cancelled}");
    assert_eq!(cancelled["result"]["isError"], true, "{cancelled}");
    assert!(cancelled["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("MCP request cancelled"));
}

#[test]
fn windowed_show_matches_cli_bytes_and_http_message_frames() {
    let mut server = Server::spawn("window-doors");
    let scope = (*cv_core::env::Env::current()).clone().enter();
    for key in cv_testkit::CLEARED_ENV {
        scope.remove(key);
    }
    for (key, value) in server.world.env() {
        scope.set(key, value);
    }
    for (flag, value, args, query) in [
        ("first", json!(0), vec!["--first", "0"], "start=0&end=0&extra=1"),
        ("first", json!(1), vec!["--first", "1"], "start=0&end=1&extra=1"),
        ("last", json!(1), vec!["--last", "1"], "start=1&end=2&extra=1"),
        ("last", json!(0), vec!["--last", "0"], "start=2&end=2&extra=1"),
        ("last", json!(100), vec!["--last", "100"], "start=0&end=2&extra=1"),
        ("range", json!("1.."), vec!["--range", "1.."], "start=1&extra=1"),
        ("range", json!("..1"), vec!["--range", "..1"], "start=0&end=1&extra=1"),
        ("max_bytes", json!(0), vec!["--max-bytes", "0"], "start=0&end=1&extra=1"),
        ("range", json!("1..2"), vec!["--range", "1..2"], "start=1&end=2&extra=1"),
        (
            "range",
            json!("9..10"),
            vec!["--range", "9..10"],
            "start=9&end=10&extra=1",
        ),
        (
            "around",
            json!(0),
            vec!["--around", "0", "--context", "0"],
            "start=0&end=1&extra=1",
        ),
    ] {
        let mut cli = vec!["show", "alphasess", "--json"];
        cli.extend(args);
        let (expected, _) = server.world.cv_ok(&cli);
        let mut arguments = json!({"id":"alphasess"});
        if flag == "around" {
            arguments["context"] = json!(0);
        }
        arguments[flag] = value;
        let (actual, error) = server.call_tool(100, "show", arguments);
        assert!(!error, "{actual}");
        assert_eq!(actual.as_bytes(), expected.as_bytes(), "{cli:?}");
        let session: Value = serde_json::from_str(&expected).unwrap();
        let staged = cv_core::api::route_staged(&["api", "session", "claude", "alphasess", "messages"], query)
            .unwrap()
            .unwrap();
        assert_eq!(staged.status, 200);
        let mut body = Vec::new();
        staged.body.copy_to(&mut body).unwrap();
        let http: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            serde_json::to_vec(&session["messages"]).unwrap(),
            serde_json::to_vec(&http["messages"]).unwrap(),
            "{query}"
        );
    }
}
