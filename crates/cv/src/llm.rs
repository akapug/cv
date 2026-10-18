//! LLM-backed features over sessions: the abstractive digest behind `cv pack` (`CV_PACK_LLM`) and
//! the `--generate` continuation of `cv splice`/`cv loom`, via OpenRouter / Anthropic / a local
//! OpenAI-compatible server. Two blocking POSTs on `ureq`; no async stack.
//!
//! Provider is chosen by environment: `LMSTUDIO_API_BASE` (local, wins), then `OPENROUTER_API_KEY`,
//! then `ANTHROPIC_API_KEY`. Remote providers receive the (truncated) transcript — which can carry
//! secrets — so every call prints an egress notice to stderr naming the destination and size.
//!
//! `digest` is the LLM summary; `cv distill` (`cv_core::distill`) is the deterministic pack. One
//! word, one meaning.

use anyhow::{anyhow, bail, Context, Result};
#[cfg(test)]
use cv_core::scan::{ceil_char, floor_char};
use serde_json::{json, Value};
use std::time::Duration;

/// Knobs for [`digest`].
#[derive(Debug, Clone, Default)]
pub(crate) struct DigestOptions {
    /// Model id (OpenRouter provider-prefixed, e.g. `anthropic/claude-…`, or an Anthropic model).
    /// Overrides the provider default when set.
    pub model: Option<String>,
    /// Digest a whole project (all sessions for a cwd) rather than a single session.
    pub project: bool,
}

/// The transcript is truncated to this many characters before being sent to the model. We keep a
/// head + tail so both the framing of the session and its conclusions survive. ~60k chars is a few
/// tens of thousands of tokens — comfortably within a small-model context while staying cheap.
const TRANSCRIPT_BUDGET: usize = 60_000;

/// System prompt steering the model toward durable memory rather than a transcript recap.
const SYSTEM_PROMPT: &str = "You are distilling an AI coding-agent session into durable project \
memory. Extract only what's worth remembering weeks later: decisions + rationale, gotchas/bugs and \
their fixes, where key things live (files/functions), conventions, and unresolved threads. Output \
terse Markdown suitable for a MEMORY.md. No fluff, no recap of the whole conversation.";

/// Which provider we'll use given the current environment, or `None` if no API key is set.
/// Returns `"openrouter"`, `"anthropic"` or `"lmstudio"`.
pub(crate) fn available_provider() -> Option<&'static str> {
    provider_from_env().map(|p| p.name())
}

/// Digest a session into a compact, durable summary of what was learned — the kind of thing you'd
/// keep in a `MEMORY.md`/`CLAUDE.md`: decisions, gotchas, where things live, open threads.
///
/// Picks a provider from the environment, builds a prompt from the rendered session, calls the
/// chat model, and returns its Markdown text.
pub(crate) fn digest(session: &cv_core::Session, opts: &DigestOptions) -> Result<String> {
    let provider = provider_from_env().ok_or_else(|| {
        anyhow!(
            "no LLM API key found: set OPENROUTER_API_KEY (preferred) or ANTHROPIC_API_KEY \
             to enable the digest"
        )
    })?;
    let prompt = build_prompt(session, opts)?;
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());
    call_model(&provider, &model, &prompt)
}

/// The provider we resolved from the environment, carrying the API key.
enum Provider {
    OpenRouter {
        key: String,
    },
    Anthropic {
        key: String,
    },
    /// A configured OpenAI-compatible server (LM Studio, Ollama, vLLM, …).
    LmStudio {
        base: String,
    },
}

impl Provider {
    fn name(&self) -> &'static str {
        match self {
            Provider::OpenRouter { .. } => "openrouter",
            Provider::Anthropic { .. } => "anthropic",
            Provider::LmStudio { .. } => "lmstudio",
        }
    }

    fn default_model(&self) -> &'static str {
        match self {
            Provider::OpenRouter { .. } => "anthropic/claude-3.5-haiku",
            Provider::Anthropic { .. } => "claude-3-5-haiku-latest",
            // LM Studio routes to the loaded model; this placeholder works when one model is loaded.
            // Pass `--model <id>` to target a specific loaded model.
            Provider::LmStudio { .. } => "local-model",
        }
    }
}

/// Tell the user (stderr) where their transcript is about to go before it leaves the machine.
/// Transcripts routinely contain secrets and file contents; egress must never be silent.
fn egress_notice(provider: &Provider, model: &str, approx_bytes: usize) {
    let kb = approx_bytes.div_ceil(1024);
    match provider {
        Provider::LmStudio { base } => {
            eprintln!(
                "✦ sending ~{kb} KB of transcript to model {model} at {base} (configured OpenAI-compatible server)"
            );
        }
        _ => {
            eprintln!(
                "✦ sending ~{kb} KB of transcript to {}/{model} — leaves this machine; \
                 transcripts can contain secrets (set LMSTUDIO_API_BASE for local/offline)",
                provider.name()
            );
        }
    }
}

/// Resolve a provider from the environment. Prefers OpenRouter when both keys are set.
fn provider_from_env() -> Option<Provider> {
    // A configured local OpenAI-compatible server (LM Studio etc.) wins — free + private. Set
    // `LMSTUDIO_API_BASE` to its base URL, or to `1`/`default`/`local` for the LM Studio default.
    if let Ok(base) = cv_core::env::var("LMSTUDIO_API_BASE") {
        let base = base.trim();
        if !base.is_empty() {
            let base = if matches!(base, "1" | "true" | "default" | "local") {
                "http://localhost:1234/v1".to_string()
            } else {
                base.trim_end_matches('/').to_string()
            };
            return Some(Provider::LmStudio { base });
        }
    }
    if let Ok(key) = cv_core::env::var("OPENROUTER_API_KEY") {
        if !key.trim().is_empty() {
            return Some(Provider::OpenRouter { key });
        }
    }
    if let Ok(key) = cv_core::env::var("ANTHROPIC_API_KEY") {
        if !key.trim().is_empty() {
            return Some(Provider::Anthropic { key });
        }
    }
    None
}

/// Build the user prompt: the (possibly truncated) rendered transcript plus instructions.
fn build_prompt(session: &cv_core::Session, opts: &DigestOptions) -> Result<String> {
    let mut output = TranscriptBudget::new(TRANSCRIPT_BUDGET);
    cv_core::render::write_markdown(session, &mut output, &session.resolver())?;
    let (transcript, truncated) = output.finish()?;

    let scope = if opts.project {
        "Distill this into durable PROJECT memory (it may span more than one session)."
    } else {
        "Distill this single session into durable project memory."
    };

    let mut prompt = String::new();
    prompt.push_str(scope);
    prompt.push_str("\n\n");
    if truncated {
        prompt.push_str(
            "NOTE: the transcript was long and has been truncated; the middle was elided and \
             replaced with a `[… transcript truncated …]` marker. Distill from what remains.\n\n",
        );
    }
    prompt.push_str("Here is the session transcript:\n\n");
    prompt.push_str("<transcript>\n");
    prompt.push_str(&transcript);
    prompt.push_str("\n</transcript>\n");
    Ok(prompt)
}

/// Retain the same head/tail byte budget as `budget` while rendering. All source fields
/// are still demanded and checked; unselected Markdown never becomes a whole-session String.
struct TranscriptBudget {
    limit: usize,
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    truncated: bool,
}
const TRANSCRIPT_MARKER: &str = "\n\n[… transcript truncated …]\n\n";
impl TranscriptBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            truncated: false,
        }
    }
    fn keep_tail(&mut self, bytes: &[u8]) {
        let usable = self.limit.saturating_sub(TRANSCRIPT_MARKER.len());
        let capacity = usable - usable * 2 / 3;
        if bytes.len() >= capacity {
            self.tail.clear();
            self.tail.extend(bytes[bytes.len() - capacity..].iter().copied());
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(capacity);
            self.tail.drain(..excess);
            self.tail.extend(bytes.iter().copied());
        }
    }
    fn finish(mut self) -> Result<(String, bool)> {
        if !self.truncated {
            return Ok((String::from_utf8(self.head)?, false));
        }
        // Head may end mid-scalar and tail may begin mid-scalar; the streamed Markdown itself
        // is UTF8. Drop only these boundary fragments, matching floor_char/ceil_char.
        if let Err(error) = std::str::from_utf8(&self.head) {
            if error.error_len().is_some() {
                bail!("Markdown renderer emitted invalid UTF8");
            }
            self.head.truncate(error.valid_up_to());
        }
        let start = self
            .tail
            .iter()
            .position(|byte| byte & 0xc0 != 0x80)
            .unwrap_or(self.tail.len());
        let mut output = String::from_utf8(self.head)?;
        output.push_str(TRANSCRIPT_MARKER);
        output.push_str(std::str::from_utf8(&self.tail.make_contiguous()[start..])?);
        Ok((output, true))
    }
}
impl std::io::Write for TranscriptBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.truncated && bytes.len() <= self.limit.saturating_sub(self.head.len()) {
            self.head.extend_from_slice(bytes);
        } else {
            if !self.truncated {
                self.truncated = true;
                let saved = std::mem::take(&mut self.head);
                self.keep_tail(&saved);
                let head_limit = self.limit.saturating_sub(TRANSCRIPT_MARKER.len()) * 2 / 3;
                self.head.extend_from_slice(&saved[..saved.len().min(head_limit)]);
                let remaining = head_limit.saturating_sub(self.head.len());
                self.head.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            }
            self.keep_tail(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Truncate `s` to roughly `budget` chars, keeping a head and a tail with an elision marker in the
/// middle. Returns `(text, was_truncated)`. Splits on char boundaries so this never panics.
#[cfg(test)]
fn budget(s: &str, budget: usize) -> (String, bool) {
    if s.len() <= budget {
        return (s.to_string(), false);
    }
    const MARKER: &str = TRANSCRIPT_MARKER;
    // Reserve room for the marker; split remaining budget head-heavy (head holds framing, tail
    // holds conclusions).
    let usable = budget.saturating_sub(MARKER.len());
    let head_len = usable * 2 / 3;
    let tail_len = usable - head_len;

    let head_end = floor_char(s, head_len);
    let tail_start = ceil_char(s, s.len().saturating_sub(tail_len));

    let mut out = String::with_capacity(budget);
    out.push_str(&s[..head_end]);
    out.push_str(MARKER);
    out.push_str(&s[tail_start..]);
    (out, true)
}

/// A blocking HTTP agent with a per-call timeout.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

/// OpenRouter asks callers to identify themselves; the two headers are harmless elsewhere.
fn openrouter_headers(req: ureq::Request, key: &str) -> ureq::Request {
    req.set("Authorization", &format!("Bearer {key}"))
        .set("HTTP-Referer", "https://github.com/emberian/cv")
        .set("X-Title", "clustervision")
}

/// Send the prompt to the model and return its text.
fn call_model(provider: &Provider, model: &str, prompt: &str) -> Result<String> {
    egress_notice(provider, model, prompt.len());
    let agent = agent(Duration::from_secs(120));

    match provider {
        Provider::OpenRouter { key } => {
            let body = json!({
                "model": model,
                "messages": [
                    { "role": "system", "content": SYSTEM_PROMPT },
                    { "role": "user", "content": prompt },
                ],
            });
            let req = openrouter_headers(agent.post("https://openrouter.ai/api/v1/chat/completions"), key);
            let v = read_json(req.send_json(body), "OpenRouter")?;
            extract_openai_text(&v, "OpenRouter")
        }
        Provider::Anthropic { key } => {
            let body = json!({
                "model": model,
                "max_tokens": 4096,
                "system": SYSTEM_PROMPT,
                "messages": [
                    { "role": "user", "content": prompt },
                ],
            });
            let req = agent
                .post("https://api.anthropic.com/v1/messages")
                .set("x-api-key", key)
                .set("anthropic-version", "2023-06-01");
            let v = read_json(req.send_json(body), "Anthropic")?;
            extract_anthropic_text(&v)
        }
        Provider::LmStudio { base } => {
            // OpenAI-compatible /chat/completions; LM Studio ignores auth, so we send none.
            let body = json!({
                "model": model,
                "messages": [
                    { "role": "system", "content": SYSTEM_PROMPT },
                    { "role": "user", "content": prompt },
                ],
            });
            let v = read_json(
                agent.post(&format!("{base}/chat/completions")).send_json(body),
                "LM Studio",
            )
            .with_context(|| format!("POST to LM Studio at {base} (is the local server running?)"))?;
            extract_openai_text(&v, "LM Studio")
        }
    }
}

/// Pull the assistant text out of an OpenAI-shaped chat response
/// (`choices[0].message.content`), with a clear error naming `who` when the shape is off.
fn extract_openai_text(v: &Value, who: &str) -> Result<String> {
    v["choices"][0]["message"]["content"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{who} response missing message content: {v}"))
}

/// Pull the assistant text out of an Anthropic Messages response: all `content[].text` blocks
/// concatenated. Empty/missing text is an error (a silent "" would read as a model refusal).
fn extract_anthropic_text(v: &Value) -> Result<String> {
    let text: String = v["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    if text.is_empty() {
        bail!("Anthropic response missing text content: {v}");
    }
    Ok(text)
}

/// Read a response body as JSON, turning HTTP error statuses into clear errors. `ureq` reports a
/// 4xx/5xx as `Error::Status` carrying the response, so the provider's own message reaches the user.
fn read_json(resp: Result<ureq::Response, ureq::Error>, who: &str) -> Result<Value> {
    let text = match resp {
        Ok(r) => r
            .into_string()
            .with_context(|| format!("reading {who} response body"))?,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            bail!("{who} API returned {code}: {text}");
        }
        Err(e) => return Err(e).with_context(|| format!("POST to {who}")),
    };
    serde_json::from_str(&text).with_context(|| format!("parsing {who} JSON response: {text}"))
}

// ---------------------------------------------------------------------------
// Generation — the generative half of looming
// ---------------------------------------------------------------------------

/// Knobs for [`generate`].
#[derive(Debug, Clone, Default)]
pub(crate) struct GenerateOptions {
    /// Model id; defaults per provider.
    pub model: Option<String>,
    /// Max tokens to generate (default 4096).
    pub max_tokens: Option<u32>,
    /// Optional system prompt to steer the continuation.
    pub system: Option<String>,
}

/// Generate the next assistant turn given a session's conversation — the generative half of looming.
/// Forking/grafting happens in [`cv_core::loom`]; this produces a fresh continuation to append. With
/// `LMSTUDIO_API_BASE` set this runs against a local model — free, offline looming.
pub(crate) fn generate(session: &cv_core::Session, opts: &GenerateOptions) -> Result<String> {
    let provider = provider_from_env().ok_or_else(|| {
        anyhow!(
            "no LLM provider: set OPENROUTER_API_KEY, ANTHROPIC_API_KEY, or LMSTUDIO_API_BASE \
             (=local for a free local LM Studio server)"
        )
    })?;
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());
    let messages = to_chat_messages(session)?;
    if messages.is_empty() {
        bail!("nothing to continue from: the session has no renderable messages");
    }
    call_chat(
        &provider,
        &model,
        opts.system.as_deref(),
        &messages,
        opts.max_tokens.unwrap_or(4096),
    )
}

/// Flatten a session's IR messages into provider chat messages (`{role, content}`), collapsing
/// content blocks to text (tool calls/results become readable notes).
fn to_chat_messages(session: &cv_core::Session) -> Result<Vec<Value>> {
    use cv_core::ir::{Block, Role};
    let reads = session.source_reads();
    let resolver = session.resolver();
    let mut out = Vec::new();
    for m in &session.messages {
        let role = match m.role {
            Role::System => "system",
            Role::Assistant => "assistant",
            // Tool results read as context for the next turn → fold into a user message.
            Role::User | Role::Tool => "user",
        };
        let mut text = String::new();
        for b in &m.content {
            match b {
                Block::Text { text: t } | Block::Thinking { text: t, .. } => {
                    t.try_for_each_chunk(&resolver, 64 * 1024, |part| {
                        text.push_str(part);
                        Ok(())
                    })?;
                    text.push('\n');
                }
                Block::ToolUse { name, input, .. } => {
                    text.push_str(&format!("[tool call: {name} "));
                    cv_core::projection::append_json(&mut text, input)?;
                    text.push_str("]\n");
                }
                Block::ToolResult { content, .. } => {
                    text.push_str("[tool result] ");
                    content.try_for_each_chunk(&resolver, 64 * 1024, |part| {
                        text.push_str(part);
                        Ok(())
                    })?;
                    text.push('\n');
                }
                Block::File { .. } | Block::Image { .. } => {}
            }
        }
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push(json!({ "role": role, "content": text }));
    }
    anyhow::ensure!(
        reads.fresh(&mut cv_core::source::Probe::default()),
        "session sources changed during chat projection"
    );
    Ok(out)
}

/// Call a provider with a full chat message list, returning the assistant text.
fn call_chat(
    provider: &Provider,
    model: &str,
    system: Option<&str>,
    messages: &[Value],
    max_tokens: u32,
) -> Result<String> {
    let approx: usize = messages.iter().map(|m| m["content"].as_str().map_or(0, str::len)).sum();
    egress_notice(provider, model, approx);
    let agent = agent(Duration::from_secs(180));

    // OpenAI-compatible providers (OpenRouter, LM Studio) share a body/parse shape.
    let openai_call = |url: String, bearer: Option<&str>| -> Result<String> {
        let mut msgs: Vec<Value> = Vec::new();
        if let Some(s) = system {
            if !s.is_empty() {
                msgs.push(json!({ "role": "system", "content": s }));
            }
        }
        msgs.extend(messages.iter().cloned());
        let body = json!({ "model": model, "messages": msgs, "max_tokens": max_tokens });
        let mut req = agent.post(&url);
        if let Some(b) = bearer {
            req = openrouter_headers(req, b);
        }
        let v = read_json(req.send_json(body), "chat").with_context(|| format!("POST to {url}"))?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("chat response missing message content: {v}"))
    };

    match provider {
        Provider::OpenRouter { key } => openai_call("https://openrouter.ai/api/v1/chat/completions".into(), Some(key)),
        Provider::LmStudio { base } => openai_call(format!("{base}/chat/completions"), None),
        Provider::Anthropic { key } => {
            let (sys, msgs) = normalize_for_anthropic(system, messages)?;
            let mut body = json!({ "model": model, "max_tokens": max_tokens, "messages": msgs });
            if let Some(sys) = sys {
                body["system"] = json!(sys);
            }
            let req = agent
                .post("https://api.anthropic.com/v1/messages")
                .set("x-api-key", key)
                .set("anthropic-version", "2023-06-01");
            let v = read_json(req.send_json(body), "Anthropic")?;
            extract_anthropic_text(&v)
        }
    }
}

/// Shape generic `{role, content}` chat messages for the Anthropic Messages API, whose contract
/// is stricter than the OpenAI shape. Pure (no HTTP) so the invariants are testable:
///
/// 1. **System hoisting** — Anthropic takes `system` as a top-level field, not a message role.
///    Any `role:"system"` messages are pulled out and joined (after the caller's `system`, if
///    given) with newlines; the returned `Option<String>` is `None` only when there's no system
///    text at all.
/// 2. **Role coalescing** — `messages` must strictly alternate user/assistant, but our IR can
///    carry runs of same-role turns. Consecutive same-role messages are merged with `\n`.
///    Non-assistant roles (tool, anything unknown) normalize to `user`.
/// 3. **Leading-user guarantee** — the first message must be `user`; a transcript opening on an
///    assistant turn gets a small synthetic user preamble. An empty message list is an error.
fn normalize_for_anthropic(system: Option<&str>, messages: &[Value]) -> Result<(Option<String>, Vec<Value>)> {
    let mut sys = system.unwrap_or("").to_string();
    let mut msgs: Vec<Value> = Vec::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "system" {
            if let Some(c) = m["content"].as_str() {
                if !sys.is_empty() {
                    sys.push('\n');
                }
                sys.push_str(c);
            }
            continue;
        }
        // Normalize any non-system role to user/assistant.
        let role = if role == "assistant" { "assistant" } else { "user" };
        let content = m["content"].as_str().unwrap_or("").to_string();
        match msgs.last_mut() {
            Some(prev) if prev["role"] == role => {
                // Coalesce: append onto the previous same-role turn.
                let merged = format!("{}\n{}", prev["content"].as_str().unwrap_or(""), content);
                prev["content"] = json!(merged);
            }
            _ => msgs.push(json!({ "role": role, "content": content })),
        }
    }
    // Anthropic requires the first message to be `user`.
    if msgs.first().map(|m| m["role"] == "assistant").unwrap_or(false) {
        msgs.insert(
            0,
            json!({ "role": "user", "content": "(continue the transcript below)" }),
        );
    }
    if msgs.is_empty() {
        bail!("nothing to send to Anthropic after normalizing messages");
    }
    Ok(((!sys.is_empty()).then_some(sys), msgs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::ir::{Block, Harness, Message, Role, Session};

    fn msg(role: Role, text: &str) -> Message {
        let mut m = Message::new(role);
        m.content.push(Block::Text {
            text: text.to_string().into(),
        });
        m
    }

    fn sample_session() -> Session {
        Session {
            messages: vec![
                msg(Role::User, "How do I fix the build?"),
                msg(Role::Assistant, "The bug lives in src/lib.rs; pin reqwest to 0.12."),
            ],
            ..Session::new("test-session", Harness::Claude)
        }
    }

    #[test]
    fn source_tool_arguments_match_owned_chat_and_reject_changed_or_deleted_source() {
        let world = crate::test_world("llm-json-boundary");
        let path = world.base.join("arguments.json");
        let value = json!({"command": "echo \"月🌙é\"\n".repeat(300), "tail": "complete argument"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let make_session = |input| {
            let mut session = sample_session();
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::ToolUse {
                id: "call".into(),
                name: "Bash".into(),
                input,
                namespace: None,
            });
            session.messages.push(message);
            session
        };
        let expected = to_chat_messages(&make_session(value.into())).unwrap();
        for deleted in [false, true] {
            std::fs::write(&path, &bytes).unwrap();
            let session = make_session(cv_core::Json::source(&path, 0, bytes.len() as u64).unwrap());
            assert_eq!(to_chat_messages(&session).unwrap(), expected);
            if deleted {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, b"{} ").unwrap();
            }
            assert!(
                to_chat_messages(&session).is_err(),
                "must reject stale arguments before egress"
            );
            assert!(build_prompt(&session, &DigestOptions::default()).is_err());
        }
    }

    #[test]
    fn chat_generation_reads_checked_text_and_results_without_materializing_session() {
        let world = crate::test_world("llm-retained-text");
        let path = world.base.join("text.json");
        let text = "λ🌙 checked text\n".repeat(1000);
        let wire = serde_json::to_vec(&text).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let source = cv_core::Json::source(&path, 0, wire.len() as u64).unwrap();
        let retained = source.with_view(|view| view.source_text()).unwrap().unwrap();
        let make = |text: cv_core::Text| {
            let mut session = sample_session();
            let mut message = Message::new(Role::Tool);
            message.content = vec![
                Block::Text { text: text.clone() },
                Block::Thinking {
                    text: text.clone(),
                    signature: None,
                    encrypted: None,
                    redacted: false,
                },
                Block::ToolResult {
                    tool_use_id: "call".into(),
                    tool_name: None,
                    status: None,
                    is_error: false,
                    details: None,
                    content: text,
                },
            ];
            session.messages.push(message);
            session
        };
        let expected = to_chat_messages(&make(text.into())).unwrap();
        let session = make(retained);
        assert_eq!(to_chat_messages(&session).unwrap(), expected);
        std::fs::remove_file(&path).unwrap();
        assert!(
            to_chat_messages(&session).is_err(),
            "generation silently omitted stale text"
        );
    }

    #[test]
    fn streamed_prompt_budget_matches_whole_render_at_chunk_and_unicode_boundaries() {
        use std::io::Write;
        for text in ["small".to_string(), "λ🌙\nbody".repeat(20_000)] {
            for chunk in [1, 3, 4096, text.len()] {
                let mut writer = TranscriptBudget::new(TRANSCRIPT_BUDGET);
                for bytes in text.as_bytes().chunks(chunk) {
                    writer.write_all(bytes).unwrap();
                }
                assert_eq!(writer.finish().unwrap(), budget(&text, TRANSCRIPT_BUDGET));
            }
        }
        let mut session = sample_session();
        session.messages.push(msg(Role::Assistant, &"λ🌙\nbody".repeat(20_000)));
        let expected = budget(&cv_core::render::to_markdown(&session).unwrap(), TRANSCRIPT_BUDGET);
        let prompt = build_prompt(&session, &DigestOptions::default()).unwrap();
        assert!(expected.1);
        assert!(prompt.contains(&format!("<transcript>\n{}\n</transcript>", expected.0)));
    }

    #[test]
    fn prompt_contains_transcript_and_instructions() {
        let s = sample_session();
        let opts = DigestOptions::default();
        let prompt = build_prompt(&s, &opts).unwrap();

        // Transcript content is present.
        assert!(prompt.contains("How do I fix the build?"));
        assert!(prompt.contains("src/lib.rs"));
        // Distillation framing is present.
        assert!(prompt.contains("durable project memory"));
        assert!(prompt.contains("<transcript>"));
        assert!(prompt.contains("</transcript>"));
        // Not truncated for a tiny session => no truncation marker.
        assert!(!prompt.contains("transcript truncated"));
    }

    #[test]
    fn project_scope_changes_framing() {
        let s = sample_session();
        let opts = DigestOptions {
            project: true,
            ..Default::default()
        };
        let prompt = build_prompt(&s, &opts).unwrap();
        assert!(prompt.contains("PROJECT memory"));
    }

    #[test]
    fn budget_truncates_head_and_tail_with_marker() {
        let big = format!("HEAD_MARKER{}TAIL_MARKER", "x".repeat(200_000));
        let (out, truncated) = budget(&big, TRANSCRIPT_BUDGET);
        assert!(truncated);
        assert!(out.len() <= TRANSCRIPT_BUDGET + 64);
        assert!(out.starts_with("HEAD_MARKER"));
        assert!(out.ends_with("TAIL_MARKER"));
        assert!(out.contains("transcript truncated"));
    }

    #[test]
    fn budget_passthrough_when_small() {
        let (out, truncated) = budget("small", TRANSCRIPT_BUDGET);
        assert!(!truncated);
        assert_eq!(out, "small");
    }

    #[test]
    fn budget_respects_char_boundaries() {
        // Multibyte chars right around the cut points must not panic or split.
        let big = format!("é{}é", "あ".repeat(100_000));
        let (out, truncated) = budget(&big, TRANSCRIPT_BUDGET);
        assert!(truncated);
        // Round-trips as valid UTF-8 (implicit: String construction would have panicked otherwise).
        assert!(out.contains("transcript truncated"));
    }

    // ───────────────────────── to_chat_messages (IR → chat shape) ─────────────────────────

    #[test]
    fn chat_messages_flatten_roles_and_blocks() {
        let mut s = sample_session();
        // A system turn, a tool turn (folds to user), an empty turn (dropped), and tool blocks.
        let mut sysm = Message::new(Role::System);
        sysm.content.push(Block::Text {
            text: "be terse".to_string().into(),
        });
        s.messages.insert(0, sysm);

        let mut toolm = Message::new(Role::Tool);
        toolm.content.push(Block::ToolResult {
            tool_use_id: "t1".into(),
            content: "exit 0".to_string().into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        s.messages.push(toolm);

        s.messages.push(Message::new(Role::User)); // no content → skipped entirely

        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::ToolUse {
            id: "t2".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}).into(),
            namespace: None,
        });
        s.messages.push(asst);

        let msgs = to_chat_messages(&s).unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // system, user, assistant, tool→user, (empty dropped), assistant(tool_use note)
        assert_eq!(roles, ["system", "user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[0]["content"], "be terse");
        assert!(msgs[3]["content"].as_str().unwrap().contains("[tool result] exit 0"));
        assert!(msgs[4]["content"].as_str().unwrap().contains("[tool call: Bash"));
    }

    // ───────────────────── Anthropic normalization (the documented invariants) ─────────────────────

    fn m(role: &str, content: &str) -> Value {
        json!({ "role": role, "content": content })
    }

    #[test]
    fn anthropic_hoists_system_messages() {
        let msgs = [m("system", "rule one"), m("user", "hi"), m("system", "rule two")];
        let (sys, out) = normalize_for_anthropic(Some("base"), &msgs).unwrap();
        assert_eq!(sys.as_deref(), Some("base\nrule one\nrule two"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], m("user", "hi"));

        // No system text anywhere → None (so the body omits the field entirely).
        let (sys, _) = normalize_for_anthropic(None, &[m("user", "hi")]).unwrap();
        assert!(sys.is_none());
    }

    #[test]
    fn anthropic_coalesces_same_role_runs() {
        let msgs = [
            m("user", "a"),
            m("user", "b"),
            m("assistant", "c"),
            m("assistant", "d"),
            m("user", "e"),
        ];
        let (_, out) = normalize_for_anthropic(None, &msgs).unwrap();
        assert_eq!(
            out,
            vec![m("user", "a\nb"), m("assistant", "c\nd"), m("user", "e")],
            "runs must merge so roles strictly alternate"
        );
    }

    #[test]
    fn anthropic_guarantees_leading_user() {
        let (_, out) = normalize_for_anthropic(None, &[m("assistant", "previously…")]).unwrap();
        assert_eq!(out[0]["role"], "user", "first message must be user");
        assert_eq!(out[1], m("assistant", "previously…"));
        // The synthesized preamble must not merge into the assistant turn.
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn anthropic_normalizes_unknown_roles_to_user_and_rejects_empty() {
        // tool/unknown roles become user (and coalesce with neighboring user turns).
        let msgs = [m("user", "run it"), m("tool", "exit 0")];
        let (_, out) = normalize_for_anthropic(None, &msgs).unwrap();
        assert_eq!(out, vec![m("user", "run it\nexit 0")]);

        // Nothing but system text → there is no conversation to send.
        assert!(normalize_for_anthropic(None, &[m("system", "rules")]).is_err());
        assert!(normalize_for_anthropic(Some("sys"), &[]).is_err());
    }

    // ───────────────────────── response extraction error paths ─────────────────────────

    #[test]
    fn openai_extraction_happy_and_sad() {
        let good = json!({"choices": [{"message": {"role": "assistant", "content": "hi"}}]});
        assert_eq!(extract_openai_text(&good, "X").unwrap(), "hi");

        for bad in [
            json!({}),
            json!({"choices": []}),
            json!({"choices": [{"message": {}}]}),
            json!({"choices": [{"message": {"content": null}}]}),
            json!({"choices": [{"message": {"content": ""}, "finish_reason":"content_filter"}]}),
            json!({"choices": [{"message": {"content": 42}}]}),
            json!({"error": {"message": "rate limited"}}),
        ] {
            let err = extract_openai_text(&bad, "TestProv").unwrap_err().to_string();
            assert!(err.contains("TestProv"), "error must name the provider: {err}");
        }
    }

    #[test]
    fn anthropic_extraction_joins_text_blocks_and_rejects_empty() {
        let good = json!({"content": [
            {"type": "text", "text": "part one "},
            {"type": "tool_use", "id": "t", "name": "x", "input": {}},
            {"type": "text", "text": "part two"},
        ]});
        assert_eq!(extract_anthropic_text(&good).unwrap(), "part one part two");

        for bad in [
            json!({}),
            json!({"content": []}),
            json!({"content": [{"type": "tool_use", "id": "t", "name": "x", "input": {}}]}),
            json!({"content": [{"type": "text", "text": ""}]}),
        ] {
            assert!(extract_anthropic_text(&bad).is_err(), "should reject: {bad}");
        }
    }

    /// Real network call. Ignored by default so `cargo test` passes without API keys / network.
    /// Run with: `cargo test -p clustervision --bin cv -- --ignored live_digest`
    #[test]
    #[ignore]
    fn live_digest() {
        let s = sample_session();
        let out = digest(&s, &DigestOptions::default()).expect("digest failed");
        assert!(!out.trim().is_empty(), "expected non-empty digest");
        eprintln!("--- digest ---\n{out}");
    }
}
