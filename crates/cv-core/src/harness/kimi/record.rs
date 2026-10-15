//! The Kimi record codec: one `context.jsonl` record view in, at most one IR message out. Every
//! byte source (mapped lines, positioned records, an owned value) feeds this one function; the
//! demand only chooses whether large strings stay in the source (`spans`), never which code runs.
//!
//! `_checkpoint` records are kimi-cli's numbered rewind targets (`Context.checkpoint()` /
//! `revert_to()`, `soul/context.py:123-200`), written at the top of every step
//! (`kimisoul.py:1036`): no content, and the conversation continues straight through them. Only a
//! later `revert_to` makes one matter, and that rewrites the file (rotating the old one to
//! `context_N.jsonl`) — so a checkpoint still present in the live transcript is one that was never
//! reverted to. Bookkeeping: a `Carrier` under `complete`, dropped otherwise; not a `Branch`, not a
//! `CompactionBoundary` (see the module doc for where those come from).

use super::*;
use crate::harness::store::document::Fields;
use crate::json::{JsonSource, JsonView};

/// One record, or `None` to skip it (`_usage` is always `None` here: the stream folds it onto
/// the preceding message with [`fold_usage`]). `spans` is the captured source when the demand
/// keeps large strings in it; `names` maps earlier tool calls to their tool.
pub(super) fn message(
    view: JsonView<'_>,
    spans: Option<&JsonSource>,
    enrich: &WireEnrich,
    complete: bool,
    names: &BTreeMap<String, String>,
) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(view, &["role", "content", "tool_calls", "tool_call_id"])? else {
        return Ok(None);
    };
    let Some(role) = fields.string("role")? else {
        return Ok(None);
    };
    let mut text = |v: JsonView<'_>| {
        if spans.is_some() {
            v.source_text()
        } else {
            v.text(false)
        }
    };
    let m = match role.as_str() {
        "_checkpoint" if complete => {
            let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
            m.extra
                .insert(crate::harness::claude::CARRIER_KEY.into(), view.resolve()?.into_owned());
            m.harness_extra_mut(Harness::Kimi)
                .insert("record_type".into(), Value::String(role));
            m
        }
        "_system_prompt" => {
            let mut m = Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness);
            m.content.push(Block::Text {
                text: join(fields.get("content"), spans)?,
            });
            m
        }
        "user" | "assistant" => {
            let mut m = if role == "user" {
                Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human)
            } else {
                Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model)
            };
            m.content = openai::blocks_from_view(fields.get("content"), &openai::KIMI, &mut text)?;
            if role == "assistant" {
                if let Some(calls) = fields.get("tool_calls").map(|v| v.array()).transpose()?.flatten() {
                    for call in calls {
                        m.content.push(openai::tool_call_from_view(call?, spans.is_some())?);
                    }
                }
            } else {
                classify(&mut m)?;
            }
            if m.content.is_empty() {
                return Ok(None);
            }
            m
        }
        "tool" => {
            let id = fields.string("tool_call_id")?.unwrap_or_default();
            // Prefer the richer wire return_value if we have it; else the context text parts.
            let (content, is_error, details) = match enrich.tool_results.get(&id) {
                Some(result) => (enrich.output(result), result.is_error, enrich.extras(result)),
                None => (join(fields.get("content"), spans)?, false, None),
            };
            let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            m.content.push(Block::ToolResult {
                tool_name: names.get(&id).cloned(),
                tool_use_id: id,
                content,
                is_error,
                status: Some(if is_error { "error" } else { "completed" }.into()),
                details,
            });
            m
        }
        _ => return Ok(None),
    };
    Ok(Some(m))
}

/// A `content` field as transcript text: a string itself, or the string `text` of each part
/// joined by newlines. Under spans a large string stays in the source and a part list is joined
/// into the source's private decoded backing, never owned.
pub(super) fn join(content: Option<JsonView<'_>>, spans: Option<&JsonSource>) -> Result<Text> {
    let Some(content) = content else {
        return Ok(Text::default());
    };
    if content.is_string()? {
        return Ok(match spans {
            Some(_) => content.source_text()?,
            None => content.text(false)?,
        }
        .unwrap_or_default());
    }
    let Some(parts) = content.array()? else {
        return Ok(Text::default());
    };
    let Some(source) = spans else {
        let mut joined = String::new();
        let mut first = true;
        for part in parts {
            if let Some(text) = part?.get("text")?.filter(|v| v.is_string().unwrap_or(false)) {
                if !first {
                    joined.push('\n');
                }
                first = false;
                joined.push_str(text.resolve()?.as_str().unwrap_or_default());
            }
        }
        return Ok(Text::Inline(joined));
    };
    crate::harness::store::text::append(source, &source.decoded_backing()?, |writer| {
        let mut first = true;
        for part in parts {
            let Some(text) = part?.get("text")? else { continue };
            if !text.is_string()? {
                continue;
            }
            if !first {
                writer.push("\n")?;
            }
            first = false;
            writer.push_string(text)?;
        }
        Ok(())
    })
}

/// `soul/compaction.py::COMPACTION_OUTPUT_PREFIX`, as `soul/message.py::system()` wraps it: the first
/// text part of the user message that seeds a rotated segment.
pub(super) const COMPACTION_SUMMARY_TAG: &str = "<system>Previous context has been compacted.";

/// Refine a `user` record's kind/origin from kimi-cli's own text markers (module doc). A typed
/// prompt never starts with these tags; the harness's `system()` / `system_reminder()` helpers and
/// `build_notification_message` always do. Only the first non-whitespace characters are read.
fn classify(message: &mut Message) -> Result<()> {
    let Some(Block::Text { text }) = message.content.first() else {
        return Ok(());
    };
    let (head, _) = crate::harness::store::document::text_head(
        text,
        &crate::lazy::Resolver::new(None),
        COMPACTION_SUMMARY_TAG.chars().count(),
        true,
    )?;
    let (kind, origin) = if head.starts_with(COMPACTION_SUMMARY_TAG) {
        (MessageKind::CompactionSummary, Origin::Harness)
    } else if head.starts_with("<notification") {
        (MessageKind::InjectedContext, Origin::Scheduler)
    } else if head.starts_with("<system>") || head.starts_with("<system-reminder>") {
        (MessageKind::InjectedContext, Origin::Harness)
    } else {
        return Ok(());
    };
    message.kind = kind;
    message.origin = origin;
    Ok(())
}

/// Fold a `{"role":"_usage","token_count":N}` record onto the message it follows: always as
/// `extra["kimi"]["token_count"]` (the record's own fact), and as `usage.input_tokens`
/// on a `Reply` the wire sidecar gave no usage for (`token_count` is the request's input tokens,
/// `kimisoul.py:1267`).
pub(super) fn fold_usage(message: &mut Message, view: JsonView<'_>) -> Result<()> {
    let Some(token) = view.get("token_count")? else {
        return Ok(());
    };
    if let Some(number) = token.as_number()? {
        let value = Value::Number(number);
        message
            .harness_extra_mut(Harness::Kimi)
            .insert("token_count".into(), value.clone());
        if message.kind == MessageKind::Reply && message.usage.is_none() {
            message.usage = Some(Usage {
                input_tokens: value.as_u64(),
                ..Usage::default()
            });
        }
    }
    Ok(())
}
