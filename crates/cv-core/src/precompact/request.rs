//! A model request as the harness sent it, read for what precompaction needs: whose session it
//! is, which text is the compaction instruction, and the conversation's prose in order.
//!
//! Three wire shapes are read by structure, not by vendor: Anthropic Messages (`messages[]` with
//! string or block `content`), OpenAI Responses (`input[]` items) and OpenAI Chat Completions
//! (`messages[]`). Tool calls, tool results and images are counted, never read as prose.

use serde_json::Value;

/// One request message, reduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `user`, `assistant`, `system`, `tool`, ...
    pub role: String,
    /// Its text blocks, in order.
    pub texts: Vec<String>,
    /// Blocks that are not text (tool calls and results, images).
    pub other: usize,
    /// A readable note per non-text block (`[tool call: …]`, `[tool result] …`, `[image]`), for an
    /// answer-time pass over the tail; never part of the prose a summary is matched on.
    pub notes: Vec<String>,
    /// Tool call ids in order, each with the number of `texts` before it.
    pub calls: Vec<(usize, String)>,
}

/// The compaction instruction: which text block of which message carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub message: usize,
    pub block: usize,
    pub text: String,
}

/// Where the session id came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId {
    pub id: String,
    /// `envelope`, `header:<name>`, `metadata.user_id`, `prompt_cache_key`.
    pub source: String,
}

/// A request, reduced.
#[derive(Debug, Clone, Default)]
pub struct RequestView {
    pub messages: Vec<Message>,
    pub instruction: Option<Instruction>,
    pub session: Option<SessionId>,
    /// The client that sent it (`User-Agent`), when the envelope carries headers.
    pub client: Option<String>,
    /// The harness the caller named, if it named one.
    pub harness: Option<String>,
}

/// The request's prose as one normalized stream, and which message each stretch came from.
pub struct Prose {
    pub text: Vec<u8>,
    /// `(offset, message)`: the message whose text starts at `offset`, ascending.
    starts: Vec<(usize, usize)>,
}

impl Prose {
    /// The message holding the stream byte at `offset`.
    pub fn message_at(&self, offset: usize) -> usize {
        match self.starts.binary_search_by(|(start, _)| start.cmp(&offset)) {
            Ok(i) => self.starts[i].1,
            Err(0) => self.starts.first().map_or(0, |s| s.1),
            Err(i) => self.starts[i - 1].1,
        }
    }
}

fn part_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The provider's id of a tool call block, if `part` is one: Anthropic `tool_use` `id`, Responses
/// `call_id`, Chat Completions `tool_calls[].id`.
fn call_id(part: &Value, kind: &str) -> Option<String> {
    let key = match kind {
        "tool_use" | "server_tool_use" => "id",
        "function_call" | "custom_tool_call" => "call_id",
        _ => return None,
    };
    part.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A non-text block as a readable note: what the tail pass reads for tool work.
fn note(part: &Value, kind: &str) -> String {
    let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
    match kind {
        "tool_use" | "server_tool_use" | "function_call" | "custom_tool_call" => {
            let args = part
                .get("input")
                .map(Value::to_string)
                .or_else(|| part.get("arguments").map(part_text))
                .unwrap_or_default();
            format!("[tool call: {name} {args}]")
        }
        "tool_result" | "function_call_output" | "custom_tool_call_output" => {
            let body = part
                .get("content")
                .or_else(|| part.get("output"))
                .map(part_text)
                .unwrap_or_default();
            format!("[tool result] {body}")
        }
        "image" | "input_image" => "[image]".into(),
        other => format!("[{other}]"),
    }
}

fn blocks(role: String, content: &Value, text_types: &[&str]) -> Message {
    let mut m = Message {
        role,
        texts: Vec::new(),
        other: 0,
        notes: Vec::new(),
        calls: Vec::new(),
    };
    match content {
        Value::String(s) => m.texts.push(s.clone()),
        Value::Array(parts) => {
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                match part.get("text").and_then(Value::as_str) {
                    Some(text) if text_types.contains(&kind) => m.texts.push(text.to_owned()),
                    _ => m.push_other(part, kind),
                }
            }
        }
        Value::Null => {}
        other => {
            m.other += 1;
            m.notes.push(other.to_string());
        }
    }
    m
}

impl Message {
    /// Count a non-text block, note it, and place its call id if it is a call.
    fn push_other(&mut self, part: &Value, kind: &str) {
        self.other += 1;
        self.notes.push(note(part, kind));
        if let Some(id) = call_id(part, kind) {
            self.calls.push((self.texts.len(), id));
        }
    }
}

/// The session id a Claude Code `metadata.user_id` carries: a JSON object string with
/// `session_id`, or the older `user_..._session_<uuid>` form.
fn user_id_session(user_id: &str) -> Option<String> {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(user_id) {
        return map.get("session_id").and_then(Value::as_str).map(str::to_owned);
    }
    user_id.rsplit_once("_session_").map(|(_, id)| id.to_owned())
}

impl RequestView {
    /// Read a request. `input` is either the request body itself or an envelope
    /// `{"body": …, "headers": {…}, "harness": …, "session": …, "instruction": …}`; the envelope's
    /// explicit fields win over what is found in the body.
    pub fn parse(input: &Value) -> anyhow::Result<RequestView> {
        let (body, envelope) = match input.get("body") {
            Some(body) if body.is_object() => (body, Some(input)),
            _ => (input, None),
        };
        let mut view = RequestView::default();
        if let Some(items) = body.get("input").and_then(Value::as_array) {
            // OpenAI Responses: message items carry `input_text`/`output_text` parts; function
            // calls and their outputs are separate items.
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("message");
                if kind != "message" {
                    if view.messages.is_empty() {
                        view.messages.push(blocks("tool".into(), &Value::Null, &[]));
                    }
                    if let Some(last) = view.messages.last_mut() {
                        last.push_other(item, kind);
                    }
                    continue;
                }
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user").to_owned();
                view.messages.push(blocks(
                    role,
                    item.get("content").unwrap_or(&Value::Null),
                    &["input_text", "output_text", "text"],
                ));
            }
        } else if let Some(messages) = body.get("messages").and_then(Value::as_array) {
            for m in messages {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("user").to_owned();
                let mut message = blocks(role, m.get("content").unwrap_or(&Value::Null), &["text"]);
                // Chat Completions carries tool calls beside the content, after it.
                for call in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let f = call.get("function").unwrap_or(call);
                    message.other += 1;
                    message.notes.push(note(f, "function_call"));
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        message.calls.push((message.texts.len(), id.to_owned()));
                    }
                }
                view.messages.push(message);
            }
        } else {
            anyhow::bail!("not a model request: neither `messages` nor `input` is an array");
        }

        let headers = envelope.and_then(|e| e.get("headers")).and_then(Value::as_object);
        let header = |name: &str| {
            headers.and_then(|h| {
                h.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .and_then(|(_, v)| match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Array(a) => a.first().and_then(Value::as_str).map(str::to_owned),
                        _ => None,
                    })
            })
        };
        view.client = header("user-agent");
        view.harness = envelope
            .and_then(|e| e.get("harness"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let explicit = envelope
            .and_then(|e| e.get("session"))
            .and_then(Value::as_str)
            .map(|id| SessionId {
                id: id.to_owned(),
                source: "envelope".into(),
            });
        view.session = explicit
            .or_else(|| {
                [
                    "x-claude-code-session-id",
                    "session_id",
                    "session-id",
                    "conversation_id",
                ]
                .iter()
                .find_map(|name| {
                    header(name).map(|id| SessionId {
                        id,
                        source: format!("header:{name}"),
                    })
                })
            })
            .or_else(|| {
                body.pointer("/metadata/user_id")
                    .and_then(Value::as_str)
                    .and_then(user_id_session)
                    .map(|id| SessionId {
                        id,
                        source: "metadata.user_id".into(),
                    })
            })
            .or_else(|| {
                body.get("prompt_cache_key")
                    .and_then(Value::as_str)
                    .map(|id| SessionId {
                        id: id.to_owned(),
                        source: "prompt_cache_key".into(),
                    })
            });

        let named = envelope
            .and_then(|e| e.get("instruction"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        view.instruction = view.find_instruction(named.as_deref());
        Ok(view)
    }

    /// The instruction: the block equal to `named` when the caller named it, else the last text
    /// block of the last user message that is not all markup (a trailing reminder is skipped).
    fn find_instruction(&self, named: Option<&str>) -> Option<Instruction> {
        let (message, m) = self.messages.iter().enumerate().rev().find(|(_, m)| m.role == "user")?;
        let block = match named {
            Some(named) => m.texts.iter().rposition(|t| t == named || t.contains(named))?,
            None => m.texts.iter().rposition(|t| !super::anchor::segments(t).is_empty())?,
        };
        let text = match named {
            Some(named) => named.to_owned(),
            None => m.texts[block].clone(),
        };
        Some(Instruction { message, block, text })
    }

    /// The conversation's prose: user and assistant text blocks, the instruction excluded,
    /// normalized and joined by single spaces, each tool call's [`call_marker`] in its place.
    ///
    /// [`call_marker`]: super::anchor::call_marker
    pub fn prose(&self) -> Prose {
        let mut text = String::new();
        let mut starts = Vec::new();
        for (i, m) in self.messages.iter().enumerate() {
            if m.role != "user" && m.role != "assistant" {
                continue;
            }
            let mut push = |text: &mut String, t: &str| {
                let before = text.len();
                super::anchor::normalize_into(text, t);
                if text.len() > before {
                    let at = if before == 0 { 0 } else { before + 1 };
                    starts.push((at, i));
                }
            };
            let mut calls = m.calls.iter().peekable();
            for (b, t) in m.texts.iter().enumerate() {
                while let Some((_, id)) = calls.next_if(|(at, _)| *at <= b) {
                    if let Some(marker) = super::anchor::call_marker(id) {
                        push(&mut text, &marker);
                    }
                }
                if self
                    .instruction
                    .as_ref()
                    .is_some_and(|ins| ins.message == i && ins.block == b)
                {
                    continue;
                }
                push(&mut text, t);
            }
            for (_, id) in calls {
                if let Some(marker) = super::anchor::call_marker(id) {
                    push(&mut text, &marker);
                }
            }
        }
        Prose {
            text: text.into_bytes(),
            starts,
        }
    }

    /// The messages from `from` on, the instruction block removed: what a summary cut before
    /// `from` does not cover. Empty when only the instruction is left.
    pub fn tail(&self, from: usize) -> Vec<Message> {
        let mut out = Vec::new();
        for (i, m) in self.messages.iter().enumerate().skip(from) {
            let mut m = m.clone();
            if let Some(ins) = self.instruction.as_ref().filter(|ins| ins.message == i) {
                if ins.block < m.texts.len() {
                    m.texts.remove(ins.block);
                }
                // A reminder after the instruction is the harness's, not conversation.
                m.texts.retain(|t| !super::anchor::segments(t).is_empty());
            }
            if m.texts.is_empty() && m.other == 0 {
                continue;
            }
            out.push(m);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_code_request_names_its_session_two_ways_and_ends_with_the_instruction() {
        let body = json!({
            "model": "m",
            "metadata": {"user_id": "{\"device_id\":\"d\",\"account_uuid\":\"\",\"session_id\":\"s-1\"}"},
            "messages": [
                {"role": "user", "content": "please refactor the parser"},
                {"role": "assistant", "content": [{"type": "text", "text": "Refactoring the parser now."}, {"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}, {"type": "text", "text": "Summarize the conversation so far."}]}
            ]
        });
        let view = RequestView::parse(&body).unwrap();
        assert_eq!(view.session.as_ref().unwrap().id, "s-1");
        assert_eq!(view.session.as_ref().unwrap().source, "metadata.user_id");
        let ins = view.instruction.as_ref().unwrap();
        assert_eq!((ins.message, ins.block), (2, 0));
        let prose = view.prose();
        assert_eq!(
            String::from_utf8(prose.text.clone()).unwrap(),
            "please refactor the parser Refactoring the parser now. \u{1}call t\u{1}"
        );
        assert_eq!(prose.message_at(0), 0);
        assert_eq!(prose.message_at(30), 1);
        assert_eq!(view.messages[1].calls, vec![(1, "t".to_owned())]);
        // The tail from the last message holds the tool result, not the instruction.
        let tail = view.tail(2);
        assert_eq!(tail.len(), 1);
        assert!(tail[0].texts.is_empty());
        assert_eq!(tail[0].other, 1);

        let enveloped =
            json!({"body": body, "headers": {"X-Claude-Code-Session-Id": "s-h", "User-Agent": "claude-cli/2"}});
        let view = RequestView::parse(&enveloped).unwrap();
        assert_eq!(view.session.unwrap().source, "header:x-claude-code-session-id");
        assert_eq!(view.client.as_deref(), Some("claude-cli/2"));
    }

    #[test]
    fn responses_input_items_are_read_by_structure() {
        let body = json!({
            "prompt_cache_key": "conv-9",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "add a test for the cache"}]},
                {"type": "function_call", "name": "shell", "call_id": "call_7", "arguments": "{}"},
                {"type": "function_call_output", "output": "done"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The cache test is added."}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Write a handoff summary."}]}
            ]
        });
        let view = RequestView::parse(&body).unwrap();
        assert_eq!(view.session.unwrap().id, "conv-9");
        assert_eq!(view.messages.len(), 3);
        assert_eq!(view.messages[0].other, 2);
        assert_eq!(view.messages[0].calls, vec![(1, "call_7".to_owned())]);
        assert_eq!(view.instruction.unwrap().text, "Write a handoff summary.");
    }
}
