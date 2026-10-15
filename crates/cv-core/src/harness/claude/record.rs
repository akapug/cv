//! The Claude record codec: one validated record view and the reader's demand in, session facts
//! and at most one IR message out. Every byte source feeds this one function: mapped lines,
//! positioned records and parsed strings differ only in the [`JsonView`] they hand it. A demand
//! (`ParseOptions` plus the sink's projection) chooses which fields are resolved, never which code
//! runs; a field nobody asked for is skipped by the view, not decoded and dropped.

use super::*;
use crate::harness::store::document::Fields;
use crate::json::JsonView;

/// Every top-level key any record kind reads. One selection pass per record; a field is
/// resolved only by the branch that needs it.
const TOP: &[&str] = &[
    "type",
    "uuid",
    "parentUuid",
    "timestamp",
    "cwd",
    "gitBranch",
    "permissionMode",
    "message",
    "isMeta",
    "isCompactSummary",
    "isApiErrorMessage",
    "scheduledTaskId",
    "scheduledFireId",
    "turnOrigin",
    "origin",
    "toolUseResult",
    "error",
    "errorDetails",
    "apiErrorStatus",
    "requestId",
    "isSnapshotUpdate",
    "snapshot",
    "trackingPath",
    "aiTitle",
    "customTitle",
    "summary",
    "agentName",
    "tag",
    "relocatedCwd",
    "continuedInSessionId",
    "prUrl",
    "attachment",
    "rendered",
    "renderedInHumanTurn",
    "agentId",
    "isSidechain",
    "subtype",
    "content",
    "compactMetadata",
    "hookCount",
    "hookInfos",
    "hookErrors",
    "hasOutput",
    "hookAdditionalContext",
    "level",
    "cause",
    "slug",
    "logicalParentUuid",
    "stopReason",
    "preventedContinuation",
    "toolUseID",
    "originalModel",
    "fallbackModel",
    "apiRefusalCategory",
    "apiRefusalExplanation",
    "retractedMessageUuids",
    "trigger",
    "direction",
    "version",
    "parentToolUseID",
    "sourceToolAssistantUUID",
    "attributionAgent",
    "attributionMcpServer",
    "attributionMcpTool",
    "attributionSkill",
    "userType",
];

/// Keys a `system` record may carry into `extra` under rich demand, in this order.
const SYSTEM_EXTRA: &[&str] = &[
    "level",
    "compactMetadata",
    "cause",
    "error",
    "isMeta",
    "isSidechain",
    "agentId",
    "slug",
    "logicalParentUuid",
    // Hook activity (Stop/PreToolUse/… hook firings): the command, its output, and errors.
    "hookCount",
    "hookInfos",
    "hookErrors",
    "stopReason",
    "preventedContinuation",
    "toolUseID",
    // Model-refusal fallback (a provider safety refusal that re-routed to another model):
    // which models, the refusal category/explanation, and what got retracted.
    "originalModel",
    "fallbackModel",
    "apiRefusalCategory",
    "apiRefusalExplanation",
    "retractedMessageUuids",
    "trigger",
    "direction",
];

/// One record: update `session` and hand its message (if any) to `sink`. `at` is the record's
/// byte offset when the source has one. `sourced` is set when the delivered message holds a
/// source descriptor, which obliges the reader to recheck the source before going on. Errors
/// are source failures, never malformed records.
pub(super) fn ingest(
    session: &mut Session,
    view: JsonView<'_>,
    at: Option<u64>,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    sourced: &mut bool,
) -> Result<Flow> {
    // A record that is not an object carries nothing (Value's lookups all miss).
    let Some(top) = Fields::parse(view, TOP)? else {
        return Ok(Flow::Continue);
    };
    let ty = top.string("type")?.unwrap_or_default();
    let ty = ty.as_str();
    let r = Record { view, top, at, opts };

    // The approval mode a prompt ran under (`permissionMode` on user rows and on `permission-mode`
    // records), and the file checkpoints `/rewind` restores: session-level affordance facts.
    if let Some(mode) = r.top.string("permissionMode")? {
        crate::affordance::note_mode(session.harness_extra_mut(Harness::Claude), &mode);
    }
    match ty {
        "file-history-snapshot" => {
            let fresh = u64::from(r.bool("isSnapshotUpdate")? != Some(true));
            let mut files = Vec::new();
            if let Some(snapshot) = r.top.get("snapshot") {
                if let Some(backups) = snapshot.get("trackedFileBackups")? {
                    backups.for_each_entry(|key, _| {
                        files.push(key.to_owned());
                        Ok(())
                    })?;
                }
            }
            let bag = session.harness_extra_mut(Harness::Claude);
            crate::affordance::note_checkpoints(bag, fresh, files.iter().map(String::as_str));
        }
        "file-history-delta" => {
            let file = r.top.string("trackingPath")?;
            crate::affordance::note_checkpoints(session.harness_extra_mut(Harness::Claude), 0, file.as_deref());
        }
        _ => {}
    }

    match ty {
        // Titles, by precedence: an explicit `/rename` (`custom-title`) outranks the model-generated
        // `ai-title`, which outranks the legacy `summary` (Claude Code stopped writing `summary`
        // records around 2.1.25x; `ai-title` is rewritten on every prompt).
        "ai-title" => {
            if let Some(t) = r.top.string("aiTitle")? {
                let renamed = session
                    .harness_extra(Harness::Claude)
                    .is_some_and(|b| b.contains_key("custom_title"));
                if !renamed {
                    session.title = Some(t);
                }
            }
            return r.carry_or_skip(sink);
        }
        "custom-title" => {
            if let Some(t) = r.top.string("customTitle")? {
                session.title = Some(t.clone());
                session
                    .harness_extra_mut(Harness::Claude)
                    .insert("custom_title".into(), Value::String(t));
            }
            return r.carry_or_skip(sink);
        }
        "summary" => {
            if session.title.is_none() {
                if let Some(t) = r.top.string("summary")? {
                    session.title = Some(t);
                }
            }
            return r.carry_or_skip(sink);
        }
        // Session-level facts. The continuation pointer is first-class lineage; the derived agent
        // name, a `/tag`, a fork's relocated cwd and the linked PR(s) are Claude facts in
        // `extra["claude"]` (snake_case: these names are cv's).
        "continued-in" => {
            if let Some(id) = r.top.string("continuedInSessionId")? {
                session.lineage.continued_in = Some(id);
            }
            return r.carry_or_skip(sink);
        }
        "agent-name" | "tag" | "relocated" | "pr-link" => {
            let (from, to) = match ty {
                "agent-name" => ("agentName", "agent_name"),
                "tag" => ("tag", "tag"),
                "relocated" => ("relocatedCwd", "relocated_cwd"),
                _ => ("prUrl", "pr_links"),
            };
            let value = r.top.string(from)?;
            let bag = session.harness_extra_mut(Harness::Claude);
            if let Some(value) = value {
                if ty == "pr-link" {
                    if let Some(list) = bag.entry(to).or_insert_with(|| Value::Array(Vec::new())).as_array_mut() {
                        list.push(Value::String(value));
                    }
                } else {
                    bag.insert(to.into(), Value::String(value));
                }
            }
            return r.carry_or_skip(sink);
        }
        // Model-visible attachments. Since ~2.1.23x the system reminders Claude Code appends to the
        // prompt — hook output, edited-file notices, queued task notifications, CLAUDE.md
        // `instructions`, skill/agent listings, … — are separate `attachment` records whose
        // `rendered[].content` is byte-for-byte what the model saw (they used to be inline
        // `<system-reminder>` text in user content). Surface those as System turns; an attachment
        // with nothing rendered (a hook that printed nothing) is bookkeeping only.
        "attachment" => {
            // `prompt_snapshot` records the whole system prompt Claude Code sent (an array of
            // sections) — the session-level fact, never a message. The latest snapshot wins: a
            // resume re-snapshots, and that is the prompt the rest of the transcript ran under.
            if let Some(attachment) = r.top.get("attachment") {
                if let Some(sections) = attachment
                    .get("systemPrompt")?
                    .map(|v| v.array())
                    .transpose()?
                    .flatten()
                {
                    let mut joined = Vec::new();
                    for section in sections {
                        if let Some(text) = section?.resolve()?.as_str() {
                            joined.push(text.to_owned());
                        }
                    }
                    let joined = joined.join("\n");
                    if !joined.trim().is_empty() {
                        session.system_prompt = Some(joined);
                    }
                }
            }
            return match r.attachment()? {
                Some(mut m) => {
                    r.stamp(&mut m);
                    Ok(deliver(m, sink, sourced))
                }
                None => r.carry_or_skip(sink),
            };
        }
        // Pure bookkeeping / live-process records with no conversational payload: the input queue,
        // UI/permission state, the leaf pointer, sub-agent hook/streaming telemetry, running cost,
        // the ATIS latch, artifact watches, request-side content replacements. The lean passes drop
        // them; `complete` carries them verbatim as round-trippable carrier messages.
        "mode"
        | "permission-mode"
        | "last-prompt"
        | "progress"
        | "started"
        | "result"
        | "queue-operation"
        | "x-quota"
        | "file-history-snapshot"
        | "cost-state"
        | "atis-latch"
        | "frame-link"
        | "content-replacement"
        | "artifact-comment-monitor"
        | "artifact-autoreact-ledger" => return r.carry_or_skip(sink),
        "user" | "assistant" | "system" | "" => {}
        // Any other record without a `message` body is a bookkeeping record we haven't met yet
        // (Claude Code adds a few per release). Never conversational — carry it in `complete` mode
        // so round-trips stay lossless, skip it otherwise.
        _ if r.top.get("message").is_none() => return r.carry_or_skip(sink),
        _ => {}
    }

    note_conversation_metadata(
        session,
        r.top.string("cwd")?.as_deref(),
        r.top.string("gitBranch")?.as_deref(),
        r.timestamp()?,
    );
    let message = match ty {
        "system" => r.system()?,
        "user" => r.conversation(Role::User, sink)?,
        "assistant" => r.conversation(Role::Assistant, sink)?,
        _ => None,
    };
    let Some(mut msg) = message else {
        return Ok(Flow::Continue);
    };
    r.stamp(&mut msg);
    // Record-local models must not depend on how much of a transcript preceded the window.
    if session.model.is_none() && msg.model.as_deref() != Some(SYNTHETIC_MODEL) {
        session.model = msg.model.clone();
    }
    Ok(deliver(msg, sink, sourced))
}

fn deliver(msg: Message, sink: &mut dyn MessageSink, sourced: &mut bool) -> Flow {
    *sourced |= msg.content.iter().any(|block| match block {
        Block::Text { text } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => {
            matches!(text, Text::Source(_))
        }
        Block::ToolUse { input, .. } => matches!(input, Json::Source(_)),
        _ => false,
    });
    sink.message(msg)
}

struct Record<'a, 'o> {
    view: JsonView<'a>,
    top: Fields<'a>,
    at: Option<u64>,
    opts: &'o ParseOptions,
}

impl<'a> Record<'a, '_> {
    fn bool(&self, key: &str) -> Result<Option<bool>> {
        Ok(self.top.get(key).map(|v| v.as_bool()).transpose()?.flatten())
    }

    fn timestamp(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self.top.string("timestamp")?.as_deref().and_then(parse_ts))
    }

    /// Offset-stamping pass: stamp the record's byte offset so `cv rewind` can map a message to
    /// its record (see [`crate::offsets::OFFSET_KEY`]). Only `cv rewind` demands it, and only a
    /// source with offsets can supply it. EVERY message-producing record must call this — an
    /// unstamped message is one rewind cannot cut at.
    fn stamp(&self, msg: &mut Message) {
        if let (true, Some(at)) = (self.opts.offsets, self.at) {
            msg.extra.insert(crate::offsets::OFFSET_KEY.into(), at.into());
        }
    }

    /// Carry the record verbatim under `complete`, skip it otherwise — the shared tail of every
    /// non-conversational record arm.
    fn carry_or_skip(&self, sink: &mut dyn MessageSink) -> Result<Flow> {
        Ok(if self.opts.complete {
            sink.message(self.carrier()?)
        } else {
            Flow::Continue
        })
    }

    /// A non-conversational meta record carried verbatim under [`ParseOptions::complete`] so a
    /// `parse → emit` round-trip reproduces it. Modeled as an empty-content `System` message —
    /// render/dataset/count all skip empty turns — with the full original record stashed under
    /// `extra["_record"]` for the emitter to replay byte-for-byte.
    fn carrier(&self) -> Result<Message> {
        let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
        m.id = self.top.string("uuid")?;
        m.parent_id = self.top.string("parentUuid")?;
        m.timestamp = self.timestamp()?;
        if let Some(t) = self.top.string("type")? {
            m.harness_extra_mut(Harness::Claude)
                .insert("record_type".into(), Value::String(t));
        }
        m.extra.insert(CARRIER_KEY.into(), self.view.resolve()?.into_owned());
        Ok(m)
    }

    fn value(&self, key: &str) -> Result<Option<Value>> {
        self.top.value(key)
    }

    /// A model-visible `attachment` record (one with a non-empty `rendered[]`) as a
    /// [`Role::System`] message: one text block per rendered item, exactly the text appended to
    /// the prompt. `None` when nothing was rendered. Under `complete` the message is also a carrier
    /// of the verbatim record, so the emitter replays it byte-for-byte while lean consumers still
    /// see its text. `origin` is [`Origin::Hook`] for `hook_*` attachments (a user-configured
    /// hook's stdout) and [`Origin::Harness`] otherwise. The attachment kind always rides in
    /// `extra["claude"]["attachment_type"]`; the full `attachment` object only under rich demand.
    fn attachment(&self) -> Result<Option<Message>> {
        let Some(rendered) = self.top.get("rendered").map(|v| v.array()).transpose()?.flatten() else {
            return Ok(None);
        };
        let Some(attachment) = self.top.get("attachment").filter(|v| v.is_object().unwrap_or(false)) else {
            return Ok(None);
        };
        let mut blocks = Vec::new();
        for item in rendered {
            let item = item?;
            let text = match item.get("content")? {
                Some(content) => content.lazy_text(self.opts.spans)?,
                None => None,
            };
            if let Some(text) = text {
                blocks.push(Block::Text { text });
            }
        }
        if blocks.is_empty() {
            return Ok(None);
        }
        let att_type = attachment
            .get("type")?
            .filter(|v| v.is_string().unwrap_or(false))
            .map(|v| v.resolve().map(|v| v.as_str().unwrap_or_default().to_owned()))
            .transpose()?
            .unwrap_or_else(|| "attachment".into());
        let mut m = if self.opts.complete {
            self.carrier()?
        } else {
            Message::new(Role::System)
        };
        m.kind = MessageKind::InjectedContext;
        m.origin = if att_type.starts_with("hook_") {
            Origin::Hook
        } else {
            Origin::Harness
        };
        m.id = self.top.string("uuid")?;
        m.parent_id = self.top.string("parentUuid")?;
        m.timestamp = self.timestamp()?;
        m.content = blocks;
        let mut rich = Vec::new();
        if self.opts.wants_full_extra() {
            rich.push(("attachment", attachment.resolve()?.into_owned()));
            for key in ["renderedInHumanTurn", "agentId"] {
                if let Some(val) = self.value(key)? {
                    rich.push((key, val));
                }
            }
            if self.bool("isSidechain")? == Some(true) {
                rich.push(("isSidechain", Value::Bool(true)));
            }
        }
        let bag = m.harness_extra_mut(Harness::Claude);
        bag.insert("attachment_type".into(), Value::String(att_type));
        for (key, val) in rich {
            bag.insert(key.into(), val);
        }
        Ok(Some(m))
    }

    /// `system` records cover a grab-bag of subtypes. Only some carry user-visible text worth
    /// surfacing as a [`Role::System`] message; the rest (timing, hook bookkeeping, killed-agent
    /// notices) have no body and are dropped. `subtype`/`level`/metadata go to `extra` either way.
    fn system(&self) -> Result<Option<Message>> {
        // Format-complete: `system` records are carried verbatim. The lean passes *render* them
        // (slash-command distillation, the synthesized `[conversation compacted]` marker, the ⛓
        // hook one-liner), which is lossy; carrying the raw record (system bodies are small
        // notices) is the only way `parse → emit` reproduces it.
        if self.opts.complete {
            return Ok(Some(self.carrier()?));
        }
        let subtype = self.top.string("subtype")?.unwrap_or_default();
        let content = self.top.get("content").filter(|v| v.is_string().unwrap_or(false));
        // `stop_hook_summary` carries no `content`, but it IS the record of a Stop-hook firing:
        // surface it as a readable line when there's something to show (output or an error).
        if subtype == "stop_hook_summary" {
            let mut hooks = Map::new();
            for key in [
                "hookCount",
                "hookErrors",
                "hasOutput",
                "hookAdditionalContext",
                "hookInfos",
            ] {
                if let Some(value) = self.value(key)? {
                    hooks.insert(key.into(), value);
                }
            }
            return match render_hook_summary(&Value::Object(hooks)) {
                Some(body) => self.system_msg(Text::Inline(body), &subtype),
                None => Ok(None),
            };
        }
        // Subtypes with no human-readable body — keep them out of the message stream.
        let bodyless = matches!(subtype.as_str(), "turn_duration" | "agents_killed");
        if bodyless || (content.is_none() && subtype != "compact_boundary") {
            return Ok(None);
        }
        // Slash-command records wrap their payload in pseudo-XML tags; distill the tag soup to a
        // readable one/two-liner (these are always small, so they stay inline).
        if subtype == "local_command" {
            if let Some(c) = content.map(|v| v.resolve()).transpose()? {
                if let Some(rendered) = render_slash_command(c.as_str().unwrap_or_default()) {
                    return self.system_msg(Text::Inline(rendered), &subtype);
                }
            }
        }
        let text = match content {
            Some(c) => c.lazy_text(self.opts.spans)?.unwrap_or_default(),
            // compact_boundary often has only structured metadata; synthesize a marker.
            None => Text::Inline("[conversation compacted]".to_string()),
        };
        self.system_msg(text, &subtype)
    }

    fn system_msg(&self, text: Text, subtype: &str) -> Result<Option<Message>> {
        // The subtype says what the notice IS: a compaction marker, an error the model never
        // answered, a Stop hook's output, a scheduled wakeup, or a plain harness notice.
        let (kind, origin) = match subtype {
            "compact_boundary" => (MessageKind::CompactionBoundary, Origin::Harness),
            "api_error" | "model_refusal_fallback" => (MessageKind::Error, Origin::Harness),
            "stop_hook_summary" => (MessageKind::Notice, Origin::Hook),
            "scheduled_task_fire" => (MessageKind::Notice, Origin::Scheduler),
            _ => (MessageKind::Notice, Origin::Harness),
        };
        let mut bag = Map::new();
        if self.opts.facts_only() {
            // Reducers read only the compaction facts: trigger and the two counters.
            if let Some(meta) = self
                .top
                .get("compactMetadata")
                .filter(|v| v.is_object().unwrap_or(false))
            {
                let mut fields = Map::new();
                if let Some(value) = meta.get("trigger")?.filter(|v| v.is_string().unwrap_or(false)) {
                    fields.insert("trigger".into(), value.resolve()?.into_owned());
                }
                for key in ["preTokens", "durationMs"] {
                    if let Some(number) = meta.get(key)?.map(|v| v.as_number()).transpose()?.flatten() {
                        if number.as_u64().is_some() {
                            fields.insert(key.into(), Value::Number(number));
                        }
                    }
                }
                bag.insert("compactMetadata".into(), Value::Object(fields));
            }
        } else if self.opts.extra {
            if !subtype.is_empty() {
                bag.insert("subtype".into(), Value::String(subtype.to_string()));
            }
            for key in SYSTEM_EXTRA {
                if let Some(val) = self.value(key)? {
                    bag.insert(key.to_string(), val);
                }
            }
        }
        let mut extra = Map::new();
        if !bag.is_empty() {
            extra.insert(Harness::Claude.as_str().into(), Value::Object(bag));
        }
        Ok(Some(Message {
            complete_record: None,
            id: self.top.string("uuid")?,
            parent_id: self.top.string("parentUuid")?,
            role: Role::System,
            kind,
            origin,
            timestamp: self.timestamp()?,
            model: None,
            content: vec![Block::Text { text }],
            usage: None,
            extra,
        }))
    }

    /// A `user` or `assistant` record. `None` when it has no `message` at all.
    fn conversation(&self, role: Role, sink: &dyn MessageSink) -> Result<Option<Message>> {
        let Some(msg) = self.top.get("message") else {
            return Ok(None);
        };
        let opts = self.opts;
        // The sink may ask for reducer facts instead of rich extras for this message.
        let facts = opts.facts_only()
            || (!opts.complete
                && opts.wants_full_extra()
                && sink.extra_projection() == crate::stream::ExtraProjection::Facts);
        let body = Fields::parse(
            msg,
            &[
                "model",
                "content",
                "usage",
                "id",
                "stop_reason",
                "stop_sequence",
                "stop_details",
            ],
        )?;
        let body_string = |key: &str| -> Result<Option<String>> { body.as_ref().map_or(Ok(None), |b| b.string(key)) };
        let model = body_string("model")?;
        let usage = match body.as_ref().and_then(|b| b.get("usage")) {
            Some(usage) => Some(parse_usage(usage)?),
            None => None,
        };
        let scheduled = self.top.get("scheduledTaskId").is_some()
            || self.top.get("scheduledFireId").is_some()
            || self.top.string("turnOrigin")?.as_deref() == Some("scheduled");
        let origin_kind = match self.top.get("origin") {
            Some(origin) => origin
                .get("kind")?
                .filter(|v| v.is_string().unwrap_or(false))
                .map(|v| v.resolve().map(|v| v.as_str().unwrap_or_default().to_owned()))
                .transpose()?,
            None => None,
        };
        let user_origin = user_origin_from(scheduled, self.bool("isMeta")? == Some(true), origin_kind.as_deref());

        let mut blocks = Vec::new();
        let mut head = None;
        let mut bare = false;
        if let Some(content) = body.as_ref().and_then(|b| b.get("content")) {
            if content.is_string()? {
                bare = true;
                if role == Role::User {
                    head = Some(classify_head(content, user_origin)?);
                }
                blocks.push(Block::Text {
                    text: content.lazy_text(opts.spans)?.unwrap_or_default(),
                });
            } else if let Some(items) = content.array()? {
                for (index, item) in items.enumerate() {
                    let item = item?;
                    if index == 0 && role == Role::User {
                        if let Some(text) = item.get("text")?.filter(|v| v.is_string().unwrap_or(false)) {
                            head = Some(classify_head(text, user_origin)?);
                        }
                    }
                    if let Some(block) = block(item, opts.spans)? {
                        blocks.push(block);
                    }
                }
            }
        }

        // INTERFACE-V2 §4: `toolUseResult` — the structured sidecar Claude writes *next to* the
        // `tool_result` block — is a SHARED concept, so it rides on the block as
        // `Block::ToolResult::details`, not in the harness bag. Only rich demand resolves it: the
        // sidecar routinely dwarfs the visible transcript. Reducers read only `restorable`.
        let sidecar = self.top.get("toolUseResult");
        if facts {
            if let Some(restorable) = sidecar.map(|v| v.get("restorable")).transpose()?.flatten() {
                attach_tool_use_result(&mut blocks, serde_json::json!({"restorable": restorable.resolve()?}));
            }
        } else if opts.wants_full_extra() && blocks.iter().any(|b| matches!(b, Block::ToolResult { .. })) {
            if let Some(sidecar) = sidecar.filter(|v| !v.is_null().unwrap_or(false)) {
                attach_tool_use_result(&mut blocks, sidecar.resolve()?.into_owned());
            }
        }

        // A `user` line carrying only tool results is really a Tool turn.
        let role =
            if role == Role::User && !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }))
            {
                Role::Tool
            } else {
                role
            };
        // A sub-agent's final answer: the `Agent`/`Task` call ran in the foreground, so its
        // sidecar names the agent and is not an `async_launched` receipt.
        let sync_agent_return = match sidecar.filter(|v| v.is_object().unwrap_or(false)) {
            Some(sidecar) => {
                sidecar.get("agentId")?.is_some()
                    && sidecar
                        .get("status")?
                        .filter(|v| v.is_string().unwrap_or(false))
                        .map(|v| v.resolve().map(|v| v.as_str() == Some("async_launched")))
                        .transpose()?
                        != Some(true)
            }
            None => false,
        };
        let api_error = self.bool("isApiErrorMessage")?;
        // Claude Code's own client-side notices (an `isApiErrorMessage`, or any `<synthetic>`
        // model row such as "No response requested.") are not turns.
        let synthetic =
            role == Role::Assistant && (api_error == Some(true) || model.as_deref() == Some(SYNTHETIC_MODEL));
        let (head_text, user) = head.unwrap_or_else(|| (String::new(), user_kind_from_head(user_origin, "")));
        let class = ConversationClass {
            synthetic,
            api_error: synthetic && api_error == Some(true),
            compact_summary: self.bool("isCompactSummary")? == Some(true),
            sync_agent_return,
            // A `user` record whose whole content is a slash-command record: the invocation or its
            // captured output. Only a bare string that *starts* with the tag qualifies.
            slash_command: bare
                && (head_text.starts_with("<command-name>") || head_text.starts_with("<local-command-stdout>")),
            user,
        };
        let (role, kind, origin) = classify_conversation(role, class, opts.complete);
        let model = if synthetic && !opts.complete { None } else { model };

        // Everything Claude-specific lands in `extra["claude"]`; the top level stays clean.
        let mut bag = Map::new();
        if synthetic && !opts.complete && !facts {
            for key in [
                "isApiErrorMessage",
                "error",
                "errorDetails",
                "apiErrorStatus",
                "requestId",
            ] {
                if let Some(val) = self.value(key)?.filter(|val| !val.is_null()) {
                    bag.insert(key.to_string(), val);
                }
            }
        }
        if facts || (!opts.extra && opts.usage_identity) {
            if let Some(id) = body_string("id")? {
                bag.insert(format!("{MESSAGE_EXTRA_PREFIX}id"), Value::String(id));
            }
        } else if opts.extra {
            for &key in CLAUDE_EXTRA_TOP {
                if let Some(value) = self.value(key)?.filter(|v| keep_extra_top(key, v)) {
                    bag.insert(key.to_owned(), value);
                }
            }
            for &key in CLAUDE_EXTRA_MSG {
                if let Some(value) = body
                    .as_ref()
                    .map(|b| b.value(key))
                    .transpose()?
                    .flatten()
                    .filter(|v| !v.is_null())
                {
                    bag.insert(format!("{MESSAGE_EXTRA_PREFIX}{key}"), value);
                }
            }
        }
        // Format-complete: capture EVERY remaining top-level + message-level field (beyond the
        // first-classed ones) so `parse → emit` reproduces the record with no dropped keys. The
        // already-captured fields are re-confirmed, not duplicated. This is also the only producer
        // of the bag's verbatim `toolUseResult`, the complete-mode replay fallback.
        if opts.complete {
            self.view.for_each_entry(|key, value| {
                if !CLAUDE_FIRSTCLASS_TOP.contains(&key) && !bag.contains_key(key) {
                    bag.insert(key.to_owned(), value.resolve()?.into_owned());
                }
                Ok(())
            })?;
            msg.for_each_entry(|key, value| {
                if CLAUDE_FIRSTCLASS_MSG.contains(&key) {
                    return Ok(());
                }
                let key = format!("{MESSAGE_EXTRA_PREFIX}{key}");
                if !bag.contains_key(&key) {
                    bag.insert(key, value.resolve()?.into_owned());
                }
                Ok(())
            })?;
        }
        let mut extra = Map::new();
        if !bag.is_empty() {
            extra.insert(Harness::Claude.as_str().into(), Value::Object(bag));
        }
        Ok(Some(Message {
            complete_record: None,
            id: self.top.string("uuid")?,
            parent_id: self.top.string("parentUuid")?,
            role,
            kind,
            origin,
            timestamp: self.timestamp()?,
            model,
            content: blocks,
            usage,
            extra,
        }))
    }
}

/// One Anthropic content block through the family's view codec, with Claude's text rule. A
/// persisted-output stub keeps the stub as `content` (that IS what the model saw) and records
/// where the full output lives, so renders can point at it and indexers can read it.
fn block(item: JsonView<'_>, spans: bool) -> Result<Option<Block>> {
    // An array result larger than a positioned read buffer is joined in place, not owned.
    let joined_over = if spans { 64 * 1024 } else { u64::MAX };
    let mut block = anthropic::block_from_view_with(
        item,
        &anthropic::ANTHROPIC,
        spans,
        &mut |v| v.lazy_text(spans),
        joined_over,
    )?;
    if let Some(Block::ToolResult { content, details, .. }) = &mut block {
        let stub = match item.get("content")?.filter(|v| v.is_string().unwrap_or(false)) {
            Some(raw) if head(raw, PERSISTED.len())? == PERSISTED => {
                persisted_output(raw.resolve()?.as_str().unwrap_or_default())
            }
            Some(_) => None,
            None => content.inline_str().and_then(persisted_output),
        };
        *details = stub.map(|(path, size)| serde_json::json!({ "persistedOutput": { "path": path, "size": size } }));
    }
    Ok(block)
}

const PERSISTED: &str = "<persisted-output>";

/// The usage counters of a `message.usage` object; anything else counts nothing.
fn parse_usage(v: JsonView<'_>) -> Result<Usage> {
    let fields = Fields::parse(
        v,
        &[
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ],
    )?;
    let get = |k: &str| -> Result<Option<u64>> {
        Ok(fields
            .as_ref()
            .and_then(|f| f.get(k))
            .map(|v| v.as_number())
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64()))
    };
    Ok(Usage {
        input_tokens: get("input_tokens")?,
        output_tokens: get("output_tokens")?,
        cache_read_tokens: get("cache_read_input_tokens")?,
        cache_creation_tokens: get("cache_creation_input_tokens")?,
        ..Usage::default()
    })
}

/// The first `chars` characters of a string after its leading whitespace, decoded in place:
/// classification never owns or windows a giant body.
fn head(view: JsonView<'_>, chars: usize) -> Result<String> {
    let mut head = String::new();
    let mut n = 0;
    view.for_each_char(|c| {
        if !(head.is_empty() && c.is_whitespace()) {
            head.push(c);
            n += 1;
        }
        n < chars
    })?;
    Ok(head)
}

/// What a `user` record's head makes it, and the head itself (for slash-command detection). A
/// harness `<task-notification>` is a sub-agent's return when its first `<summary>` names an agent
/// or a workflow; that summary can follow a giant output, so it is found by streaming.
fn classify_head(text: JsonView<'_>, origin: Origin) -> Result<(String, (MessageKind, Origin))> {
    let prefix = head(text, 64)?;
    let kind = if origin == Origin::Harness && prefix.starts_with("<task-notification>") {
        let mut summary = NotificationSummary::default();
        text.for_each_str_chunk(|chunk| {
            summary.push(chunk);
            Ok(())
        })?;
        if summary.complete && ((summary.agent_possible && summary.agent == 6) || summary.workflow_found) {
            (MessageKind::SubagentReturn, Origin::Subagent)
        } else {
            (MessageKind::InjectedContext, Origin::Harness)
        }
    } else {
        user_kind_from_head(origin, &prefix)
    };
    Ok((prefix, kind))
}

/// tag_inner's first `<summary>` and its first following `</summary>`, across decoder windows,
/// with bounded state: tag and word suffixes are carried, the summary itself never copied.
struct NotificationSummary {
    opened: bool,
    complete: bool,
    pending: String,
    scratch: String,
    leading: bool,
    agent_possible: bool,
    agent: usize,
    workflow_found: bool,
    workflow: usize,
}
impl Default for NotificationSummary {
    fn default() -> Self {
        Self {
            opened: false,
            complete: false,
            pending: String::new(),
            scratch: String::new(),
            leading: true,
            agent_possible: true,
            agent: 0,
            workflow_found: false,
            workflow: 0,
        }
    }
}
impl NotificationSummary {
    fn push(&mut self, chunk: &str) {
        if self.complete {
            return;
        }
        self.scratch.clear();
        self.scratch.push_str(&self.pending);
        self.scratch.push_str(chunk);
        self.pending.clear();
        let mut body = self.scratch.as_str();
        if !self.opened {
            if let Some(at) = body.find("<summary>") {
                self.opened = true;
                body = &body[at + 9..];
            } else {
                let n = suffix(body.as_bytes(), b"<summary>", false);
                self.pending.push_str(&body[body.len() - n..]);
                return;
            }
        }
        if let Some(at) = body.find("</summary>") {
            body = &body[..at];
            self.complete = true;
        } else {
            let n = suffix(body.as_bytes(), b"</summary>", false);
            self.pending.push_str(&body[body.len() - n..]);
            body = &body[..body.len() - n];
        }
        if self.leading {
            body = body.trim_start();
            self.leading = body.is_empty();
        }
        if self.agent_possible && self.agent < 6 {
            let n = body.len().min(6 - self.agent);
            if body.as_bytes()[..n] == b"Agent "[self.agent..self.agent + n] {
                self.agent += n;
            } else {
                self.agent_possible = false;
            }
        }
        if !self.workflow_found {
            const WORD: &[u8] = b"workflow";
            // Only a proper suffix can bridge windows. Never copy the complete summary.
            let mut bridge = [0; 14];
            bridge[..self.workflow].copy_from_slice(&WORD[..self.workflow]);
            let n = body.len().min(7);
            bridge[self.workflow..self.workflow + n].copy_from_slice(&body.as_bytes()[..n]);
            let bridge = &bridge[..self.workflow + n];
            let contains = |bytes: &[u8]| {
                memchr::memchr2_iter(b'w', b'W', bytes).any(|at| {
                    bytes
                        .get(at..at + 8)
                        .is_some_and(|word| word.eq_ignore_ascii_case(WORD))
                })
            };
            self.workflow_found = contains(bridge) || contains(body.as_bytes());
            self.workflow = suffix(if body.len() < 7 { bridge } else { body.as_bytes() }, WORD, true);
        }
    }
}

/// Longest proper suffix matching an ASCII tag/word prefix (therefore a UTF-8 boundary).
fn suffix(body: &[u8], wanted: &[u8], insensitive: bool) -> usize {
    (1..wanted.len().min(body.len() + 1))
        .rev()
        .find(|&n| {
            let end = &body[body.len() - n..];
            if insensitive {
                end.eq_ignore_ascii_case(&wanted[..n])
            } else {
                end == &wanted[..n]
            }
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle(body: &str, origin: Origin) -> (MessageKind, Origin) {
        let head = body.trim_start();
        if head.starts_with("[Request interrupted by user") {
            (MessageKind::InjectedContext, Origin::Harness)
        } else if origin != Origin::Harness {
            (MessageKind::Prompt, origin)
        } else if head.starts_with("<task-notification>")
            && tag_inner(head, "summary").is_some_and(|summary| {
                let summary = summary.trim_start();
                summary.starts_with("Agent ") || summary.to_ascii_lowercase().contains("workflow")
            })
        {
            (MessageKind::SubagentReturn, Origin::Subagent)
        } else {
            (MessageKind::InjectedContext, Origin::Harness)
        }
    }

    /// The bounded head and streamed summary classify exactly as the whole decoded string would,
    /// in owned and source views, plain and escaped (including escaped tags).
    #[test]
    fn bounded_classification_matches_whole_string_oracle() {
        let mut bodies = vec![
            "<task-notification><summary>Agent a completed</summary>".to_string(),
            "<task-notification><summary>agent a completed</summary>".into(),
            "<task-notification><summary>  \u{2003}Agent a completed</summary>".into(),
            "<task-notification><summary>WoRkFlOw completed</summary>".into(),
            "<task-notification><summary>work</summary>flow".into(),
            "<task-notification>workflow<summary>command done</summary>".into(),
            "<task-notification><summary>command done</summary><summary>Agent a</summary>".into(),
            "<task-notification><summary>Agent a".into(),
            "<task-notification><summary>workflow</Summary>".into(),
            "<task-notification><summary></summary><summary>workflow</summary>".into(),
            "<task-notification></summary><summ<summary>workflow</summary>".into(),
            "<task-notification><summary><summary>Agent a</summary></summary>".into(),
            "<task-notification><summary>wo🦝rkflow</summary>".into(),
            "<task-notification><summary>wwoWorkflow done</summary>".into(),
            "[Request interrupted by user]".into(),
            "<command-name>/help</command-name>".into(),
        ];
        for summary in ["Agent completed", "worKflow done", "command done", "Agent unclosed"] {
            let close = if summary.ends_with("unclosed") {
                ""
            } else {
                "</summary>"
            };
            bodies.push(format!(
                "\u{2003}<task-notification>{}<summary>{summary}{close}",
                "ignored\n🦝".repeat(20_000)
            ));
        }
        let dir = cv_testkit::temp_dir("claude-classification");
        let path = dir.join("text.json");
        for body in bodies {
            for origin in [Origin::Harness, Origin::Human, Origin::Scheduler] {
                let expected = oracle(&body, origin);
                let value = Value::String(format!("{body}\n"));
                assert_eq!(classify_head((&value).into(), origin).unwrap().1, expected);
                for encoded in [
                    serde_json::to_string(&value).unwrap(),
                    serde_json::to_string(&value).unwrap().replace('<', r"\u003c"),
                ] {
                    fs::write(&path, &encoded).unwrap();
                    let source = JsonSource::capture(&path).unwrap();
                    let actual = source
                        .read_record(0, &encoded, |view| classify_head(view, origin))
                        .unwrap();
                    assert_eq!(actual.1, expected, "{origin:?}");
                }
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notification_summary_crosses_windows() {
        for body in [
            "🦝<summary>\u{2003}Agent completed</summary>outside workflow",
            "<summary>woRkFlOw done</summary>",
            "<summary>work</summary>flow<summary>Agent ignored</summary>",
            "<summary>Agent unclosed",
            "<summary><summary>workflow</summary>",
        ] {
            let expected = tag_inner(body, "summary").is_some_and(|summary| {
                let summary = summary.trim_start();
                summary.starts_with("Agent ") || summary.to_ascii_lowercase().contains("workflow")
            });
            for width in 1..24 {
                let mut summary = NotificationSummary::default();
                let mut start = 0;
                while start < body.len() {
                    let mut end = (start + width).min(body.len());
                    while !body.is_char_boundary(end) {
                        end += 1;
                    }
                    summary.push(&body[start..end]);
                    start = end;
                }
                assert_eq!(
                    summary.complete && ((summary.agent_possible && summary.agent == 6) || summary.workflow_found),
                    expected,
                    "width {width}"
                );
            }
        }
    }
}
