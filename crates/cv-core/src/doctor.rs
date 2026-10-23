//! Compaction doctor: attribute a session's context-window pressure to its sources.
//!
//! Powers both `cv doctor` (CLI rendering) and the `doctor` MCP tool. Sizes stored transcript
//! content with the shared estimator ([`crate::ir::est_tokens`]: per-kind bytes/token calibrated
//! against the API's token counts). Thinking counts plaintext plus signature and encrypted bytes,
//! which Claude requests do carry. Totals span transcript history, including compactions, rather
//! than describing the currently loaded model context.
//!
//! Recorded prompt usage supplies peak sizing and the first-prompt heuristic for fixed overhead.
//! The first prompt also contains conversation; fixed system/tool overhead is not separately
//! measured or itemized. Recorded usage may itself have been rewritten by a transcript transform.

use crate::compaction::CompactionSink;
use crate::ir::{est_tokens_resolved, is_mcp_tool, Block, Message, MessageKind, Role};
use crate::stream::{Flow, MessageSink};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

/// The bucket name for an injected-context turn: the harness's own attachment kind when it recorded
/// one ([`crate::harness::Adapter::attachment_kind`], e.g. Claude's `hook_success`), else the IR
/// kind name.
fn injected_kind(m: &Message) -> String {
    crate::harness::attachment_kind(m)
        .map(str::to_string)
        .unwrap_or_else(|| m.kind.as_str().to_string())
}

#[derive(Default, Debug, Clone, Serialize)]
pub struct ToolStat {
    pub calls: u64,
    pub result_tokens: u64,
    pub is_mcp: bool,
}

/// A context-attribution report. Accumulate sessions with [`Report::session`], then render
/// (CLI) or serialize ([`Report::to_json`], for `--json` and the MCP tool).
#[derive(Default, Debug, Clone, Serialize)]
pub struct Report {
    pub sessions: u64,
    pub messages: u64,
    // Estimated token equivalents of cumulative stored content, by source
    pub user_text: u64,
    pub assistant_text: u64,
    /// Stored thinking text plus opaque signature/encrypted payload byte estimates.
    pub thinking: u64,
    pub tool_call_args: u64,
    pub tool_results: u64,
    pub images: u64,
    pub system_text: u64,
    /// System reminders Claude Code appended to the prompt (model-visible `attachment` records).
    pub attachments: u64,
    /// The same, by attachment kind (`hook_success`, `edited_text_file`, `queued_command`, …).
    pub by_attachment: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, ToolStat>,
    // compaction
    pub compactions: u64,
    pub auto_compactions: u64,
    pub pre_tokens: Vec<u64>,
    // usage-derived context sizing
    pub startup_ctx: u64, // maximum first nonzero recorded prompt usage; includes conversation
    pub peak_ctx: u64,    // maximum recorded prompt usage across turns
}

impl Report {
    /// Start folding one session into the report. Stream its messages into the returned sink (a
    /// [`Demand::FACTS`](crate::Demand::FACTS) pass: attribution counts
    /// decoded text in bounded chunks, and attachment/compaction metadata live in `extra`), then
    /// call [`SessionPass::finish`]. Adapters without a facts projection retain their full metadata.
    pub fn session(&mut self, source: Option<std::path::PathBuf>) -> SessionPass<'_> {
        self.sessions += 1;
        SessionPass {
            rep: self,
            resolver: crate::lazy::Resolver::new(source),
            names: HashMap::new(),
            unnamed: HashMap::new(),
            startup: None,
            compactions: CompactionSink::new(false),
            latch: Default::default(),
        }
    }

    /// Total estimated token equivalents across stored transcript history, including compactions.
    pub fn conv_total(&self) -> u64 {
        self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
            + self.attachments
    }

    /// Tool-result tokens attributable to MCP tools ([`is_mcp_tool`]).
    pub fn mcp_tool_results(&self) -> u64 {
        self.by_tool
            .values()
            .filter(|s| s.is_mcp)
            .map(|s| s.result_tokens)
            .sum()
    }

    /// The machine-readable diagnosis — `cv doctor --json` and the MCP `doctor` tool share this.
    /// `conversational` and tool/attachment values are cumulative
    /// byte-based estimates, not measured provider tokens or a current-window breakdown. `thinking`
    /// includes opaque payloads. `peak_context` is maximum recorded prompt usage;
    /// `fixed_overhead_est` is the maximum first nonzero prompt usage across sessions and includes
    /// conversation, so it is only a heuristic proxy for unitemized fixed overhead.
    pub fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .by_tool
            .iter()
            .map(|(name, s)| json!({"tool": name, "mcp": s.is_mcp, "calls": s.calls, "result_tokens": s.result_tokens}))
            .collect();
        let mcp = self.mcp_tool_results();
        json!({
            "sessions": self.sessions,
            "messages": self.messages,
            "compactions": self.compactions,
            "auto_compactions": self.auto_compactions,
            "pre_tokens": self.pre_tokens,
            "fixed_overhead_est": self.startup_ctx,
            "peak_context": self.peak_ctx,
            "conversational": {
                "total": self.conv_total(),
                "tool_results": self.tool_results,
                "thinking": self.thinking,
                "assistant_text": self.assistant_text,
                "user_text": self.user_text,
                "tool_call_args": self.tool_call_args,
                "images": self.images,
                "system_msgs": self.system_text,
                "system_reminders": self.attachments,
            },
            "by_attachment": self.by_attachment,
            "tool_results_mcp": mcp,
            "tool_results_builtin": self.tool_results.saturating_sub(mcp),
            "by_tool": tools,
        })
    }
}

/// One session streaming into a [`Report`] (see [`Report::session`]): content attribution, the
/// usage-derived context sizing and the compaction tally, folded per message.
pub struct SessionPass<'a> {
    rep: &'a mut Report,
    resolver: crate::lazy::Resolver,
    /// `tool_use_id` → tool name, the last call per id wins. A result usually carries only the
    /// id, so results attribute to real tools (and MCP names), not "(unknown tool)".
    names: HashMap<String, String>,
    /// Result tokens by `tool_use_id` for results that named no tool, attributed at
    /// [`finish`](SessionPass::finish): a call can stream past after its result (a resumed
    /// transcript re-copies history), so the id is resolved against the whole session.
    unnamed: HashMap<String, u64>,
    /// First nonzero recorded prompt usage, including conversation.
    startup: Option<u64>,
    compactions: CompactionSink,
    latch: crate::stream::Latch,
}

impl SessionPass<'_> {
    /// Fold the session's compactions and fixed-overhead estimate into the report.
    pub fn finish(mut self) -> anyhow::Result<()> {
        if let Some(error) = self.latch.take("doctor source read") {
            return Err(error);
        }
        let rep = self.rep;
        for (id, t) in self.unnamed {
            let name = self.names.get(&id).map_or("(unknown tool)", String::as_str);
            let e = rep.by_tool.entry(name.to_string()).or_default();
            e.result_tokens += t;
            e.is_mcp |= is_mcp_tool(name);
        }
        for c in self.compactions.into_boundaries()? {
            rep.compactions += 1;
            if c.trigger.as_deref() == Some("auto") {
                rep.auto_compactions += 1;
            }
            if let Some(p) = c.pre_tokens {
                rep.pre_tokens.push(p);
            }
        }
        rep.startup_ctx = rep.startup_ctx.max(self.startup.unwrap_or(0));
        Ok(())
    }

    fn observe(&mut self, m: &Message) -> anyhow::Result<()> {
        let rep = &mut *self.rep;
        rep.messages += 1;
        if let Some(u) = &m.usage {
            let total = u.prompt_tokens();
            if total > 0 {
                rep.peak_ctx = rep.peak_ctx.max(total);
                self.startup.get_or_insert(total);
            }
        }
        // Harness-only records can carry any block type. They remain transcript/usage and
        // compaction evidence, but none of their payload contributes to model context.
        if !m.is_model_visible() {
            return Ok(());
        }
        for b in &m.content {
            // What this block costs, by the one shared rule; the arms below only decide which
            // bucket the number lands in.
            let tokens = || est_tokens_resolved(b, &self.resolver);
            match b {
                Block::Text { .. } => {
                    match (m.role, m.kind) {
                        // Context the harness injected into the prompt (system reminders, hook
                        // output, turn context): real per-turn context, itemized by kind — it
                        // used to hide inside "fixed overhead".
                        (_, MessageKind::InjectedContext) => {
                            let t = tokens()?;
                            rep.attachments += t;
                            *rep.by_attachment.entry(injected_kind(m)).or_default() += t;
                        }
                        (Role::Assistant, _) => rep.assistant_text += tokens()?,
                        (Role::System, _) => rep.system_text += tokens()?,
                        _ => rep.user_text += tokens()?,
                    }
                }
                Block::Thinking { .. } => rep.thinking += tokens()?,
                Block::ToolUse { id, name, .. } => {
                    rep.tool_call_args += tokens()?;
                    self.names.insert(id.clone(), name.clone());
                    let e = rep.by_tool.entry(name.clone()).or_default();
                    e.calls += 1;
                    e.is_mcp |= is_mcp_tool(name);
                }
                Block::ToolResult {
                    tool_name, tool_use_id, ..
                } => {
                    let t = tokens()?;
                    rep.tool_results += t;
                    match tool_name {
                        Some(name) => {
                            let e = rep.by_tool.entry(name.clone()).or_default();
                            e.result_tokens += t;
                            e.is_mcp |= is_mcp_tool(name);
                        }
                        None => *self.unnamed.entry(tool_use_id.clone()).or_default() += t,
                    }
                }
                Block::Image { .. } => rep.images += tokens()?,
                Block::File { .. } => {}
            }
        }
        Ok(())
    }
}

impl MessageSink for SessionPass<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("doctor source read")
    }

    fn message(&mut self, m: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        if let Err(error) = self.observe(&m) {
            self.latch.fail(error);
            return Flow::Stop;
        }
        self.compactions.message(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, body: &str) -> Message {
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: id.into(),
            content: body.to_string().into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        m
    }

    fn call(id: &str, name: &str) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: json!({}).into(),
            namespace: None,
        });
        m
    }

    #[test]
    fn unreadable_text_cannot_be_published_as_zero_tokens() {
        let world = crate::test_world("doctor-checked-text");
        let path = world.base.join("payload.json");
        let wire = serde_json::to_vec(&"🌙\n".repeat(8192)).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let retained = crate::json::Json::source(&path, 0, wire.len() as u64)
            .unwrap()
            .with_view(|view| view.source_text())
            .unwrap()
            .unwrap();
        let span = crate::lazy::Text::Span(crate::lazy::Span {
            source: Some(path.clone()),
            offset: 1,
            len: wire.len() as u64 - 2,
            escaped: true,
        });
        std::fs::remove_file(path).unwrap();
        for text in [retained, span] {
            let mut report = Report::default();
            let mut pass = report.session(None);
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::Text { text });
            assert!(matches!(pass.message(message), Flow::Stop));
            assert!(pass.finish().is_err());
        }
    }

    /// A result that names no tool attributes to its call's tool even when the call streams past
    /// after it (a resumed transcript re-copies history); an id no call carries is unknown.
    #[test]
    fn unnamed_results_resolve_against_the_whole_session() {
        let mut rep = Report::default();
        let mut pass = rep.session(None);
        for m in [
            result("t1", &"x".repeat(70)),
            call("t1", "mcp__s__q"),
            result("t9", "y"),
        ] {
            let _ = pass.message(m);
        }
        pass.finish().unwrap();
        // Attribution, not estimator calibration: the 70-char result outsizes the 1-char one,
        // and the MCP split follows the tool names.
        let mcp = &rep.by_tool["mcp__s__q"];
        assert_eq!((mcp.calls, mcp.is_mcp), (1, true));
        assert!(mcp.result_tokens > rep.by_tool["(unknown tool)"].result_tokens);
        assert_eq!(rep.mcp_tool_results(), mcp.result_tokens);
        assert_eq!((rep.sessions, rep.messages), (1, 3));
    }
    #[test]
    fn harness_only_payloads_never_contribute_context_or_tool_attribution() {
        let mut report = Report::default();
        let mut pass = report.session(None);
        for kind in [
            MessageKind::Notice,
            MessageKind::Error,
            MessageKind::Carrier,
            MessageKind::CompactionBoundary,
            MessageKind::Branch,
            MessageKind::ModelChange,
        ] {
            let mut message = Message::of_kind(Role::Assistant, kind, crate::Origin::Harness);
            message.content = vec![
                Block::Text {
                    text: "harness text".into(),
                },
                Block::Thinking {
                    text: "harness thought".into(),
                    signature: Some("signature".into()),
                    encrypted: Some("opaque".into()),
                    redacted: false,
                },
                Block::ToolUse {
                    id: "hidden".into(),
                    name: "mcp__hidden__tool".into(),
                    input: json!({"argument":"hidden"}).into(),
                    namespace: None,
                },
                Block::ToolResult {
                    tool_use_id: "hidden".into(),
                    tool_name: Some("mcp__hidden__tool".into()),
                    content: "harness result".into(),
                    is_error: false,
                    status: None,
                    details: None,
                },
                Block::Image {
                    media_type: None,
                    data_ref: None,
                },
            ];
            assert_eq!(pass.message(message), Flow::Continue);
        }
        pass.finish().unwrap();
        assert_eq!(report.messages, 6);
        assert_eq!(report.conv_total(), 0);
        assert!(report.by_tool.is_empty());
        assert!(report.by_attachment.is_empty());
        // Model-visible injected content still contributes to the appropriate buckets.
        let mut pass = report.session(None);
        let mut visible = Message::of_kind(Role::User, MessageKind::InjectedContext, crate::Origin::Harness);
        visible.content.push(Block::Text {
            text: "reminder".into(),
        });
        pass.message(visible);
        pass.finish().unwrap();
        assert!(report.attachments > 0);
    }
}
