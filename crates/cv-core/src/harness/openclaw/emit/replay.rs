//! Unedited native records keep their fields and tree links while moving to a new session.
use super::*;
use crate::{
    harness::wire::write::{self as wire, ObjectWriter},
    json::JsonView,
    lazy::Resolver,
    stream::Demand,
    Json, Message,
};
use std::collections::HashMap;
use std::io::Write;

pub(super) struct Native {
    enabled: bool,
    linked: bool,
    ids: HashMap<String, String>,
    namespace: Uuid,
    model: Option<String>,
}
impl Native {
    pub(super) fn new(session: &Session, new_id: &str) -> Self {
        let namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, new_id.as_bytes());
        let enabled = session.harness == Harness::OpenClaw;
        let header_version = session
            .extra
            .get("openclaw")
            .and_then(|v| v.get("header_record"))
            .and_then(|v| v.get("version"))
            .and_then(Value::as_i64)
            .or_else(|| {
                session.messages.iter().find_map(|m| {
                    m.extra
                        .get("openclaw")
                        .and_then(|v| v.get("session_version"))
                        .and_then(Value::as_i64)
                })
            });
        let mut ids = HashMap::new();
        if enabled {
            for id in session.messages.iter().filter_map(|m| m.id.as_ref()) {
                ids.entry(id.clone())
                    .or_insert_with(|| openclaw_short_id(&namespace, &format!("native:{id}")));
            }
        }
        Self {
            enabled,
            linked: enabled && header_version.is_some_and(|v| v >= 3),
            ids,
            namespace,
            model: session.model.clone(),
        }
    }
    pub(super) fn generated_id(&self, key: &str) -> String {
        openclaw_short_id(&self.namespace, key)
    }
    pub(super) fn entry_id(&self, message: &Message, index: usize) -> String {
        message
            .id
            .as_ref()
            .and_then(|id| self.ids.get(id))
            .cloned()
            .unwrap_or_else(|| self.generated_id(&format!("message:{index}")))
    }
    pub(super) fn parent(&self, message: &Message, linear: &Option<String>) -> Option<String> {
        if self.linked && message.id.is_some() {
            message
                .parent_id
                .as_ref()
                .map(|id| self.ids.get(id).unwrap_or(id).clone())
        } else {
            linear.clone()
        }
    }
    pub(super) fn has_title_record(&self, session: &Session) -> bool {
        self.enabled
            && session.messages.iter().any(|m| {
                m.extra
                    .get("openclaw")
                    .and_then(|v| v.get("record_type"))
                    .and_then(Value::as_str)
                    == Some("session_info")
            })
    }
    pub(super) fn header(&self, session: &Session, writer: &mut dyn Write, new_id: &str, cwd: &str) -> Result<bool> {
        let header = self
            .enabled
            .then(|| session.extra.get("openclaw").and_then(|v| v.get("header_record")))
            .flatten();
        let Some(Value::Object(header)) = header else {
            return Ok(false);
        };
        let mut out = ObjectWriter::new(writer)?;
        let original_time = header
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::harness::parse_ts);
        let original_parent = header.get("parentSession").and_then(Value::as_str);
        for (key, value) in header {
            if matches!(key.as_str(), "id" | "cwd")
                || (key == "timestamp" && original_time != session.created_at)
                || (key == "parentSession" && original_parent != session.lineage.forked_from.as_deref())
            {
                continue;
            }
            out.field(key, value)?;
        }
        if original_time != session.created_at {
            if let Some(created) = session.created_at {
                out.field("timestamp", created.to_rfc3339_opts(SecondsFormat::Millis, true))?;
            }
        }
        if original_parent != session.lineage.forked_from.as_deref() {
            if let Some(parent) = &session.lineage.forked_from {
                out.field("parentSession", parent)?;
            }
        }
        out.field("id", new_id)?;
        if !cwd.is_empty() {
            out.field("cwd", cwd)?;
        }
        out.finish()?;
        Ok(true)
    }
    pub(super) fn record(
        &self,
        messages: &[Message],
        writer: &mut dyn Write,
        mode: ThinkingMode,
        resolver: &Resolver,
    ) -> Result<Option<usize>> {
        if !self.enabled {
            return Ok(None);
        }
        let message = messages.first().context("OpenClaw replay requires a current message")?;
        if !message.extra.contains_key(crate::ir::CARRIER_KEY) {
            if let Some(record) = &message.complete_record {
                let group = if record.unchanged(Harness::OpenClaw, messages) {
                    Some(record.projection.len())
                } else if let Json::Owned(value) = &record.value {
                    self.owned_group(value, &record.projection, messages)?
                } else {
                    None
                };
                if let Some(group) = group {
                    let written = record.value.with_view(|view| {
                        if message.role == Role::Assistant {
                            if let Some(model) = view.get("message")?.map(|v| v.get("model")).transpose()?.flatten() {
                                if model.is_string()?
                                    && model.resolve()?.as_str() != message.model.as_deref().or(self.model.as_deref())
                                {
                                    return Ok(false);
                                }
                            }
                        }
                        self.write(view, writer, mode, resolver)?;
                        Ok(true)
                    })?;
                    if written {
                        return Ok(Some(group));
                    }
                }
            }
        }
        // The public IR representation also accepts explicitly owned bookkeeping carriers.
        if matches!(message.kind, MessageKind::Carrier | MessageKind::Notice) {
            if let Some(raw) = message.extra.get(crate::ir::CARRIER_KEY) {
                self.write(raw.into(), writer, mode, resolver)?;
                return Ok(Some(1));
            }
        }
        Ok(None)
    }
    // A full owned read has no retained payload witness: rebuild just this record using
    // the native codec, then apply the session-level normalization used by the reader.
    fn owned_group(&self, value: &Value, envelopes: &[Message], messages: &[Message]) -> Result<Option<usize>> {
        let mut original = super::super::record::record(JsonView::from(value), &Demand::COMPLETE)?.messages;
        if original.len() != envelopes.len() || messages.len() < original.len() {
            return Ok(None);
        }
        for ((original, envelope), current) in original.iter_mut().zip(envelopes).zip(messages) {
            if !crate::ir::record::same_envelope(envelope, current) {
                return Ok(None);
            }
            let original = Message {
                content: std::mem::take(&mut original.content),
                ..envelope.clone()
            };
            if !crate::ir::record::same_message(&original, current) {
                return Ok(None);
            }
        }
        Ok(Some(original.len()))
    }
    fn write(
        &self,
        view: JsonView<'_>,
        mut writer: &mut dyn Write,
        mode: ThinkingMode,
        resolver: &Resolver,
    ) -> Result<()> {
        if !view.is_object()? {
            return view.write_canonical(&mut writer);
        }
        let mut out = ObjectWriter::new(writer)?;
        view.for_each_entry(|key, value| {
            if matches!(
                key,
                "id" | "parentId" | "targetId" | "appendParentId" | "firstKeptEntryId" | "fromId"
            ) && value.is_string()?
            {
                if let Some(id) = value.resolve()?.as_str().and_then(|id| self.ids.get(id)) {
                    return out.field(key, id);
                }
            }
            if key == "message" && mode != ThinkingMode::Native && value.is_object()? {
                return out.field_with(key, |writer| message_with_thinking(value, writer, mode, resolver));
            }
            out.field_with(key, |mut writer| value.write_canonical(&mut writer))
        })?;
        out.finish()
    }
}

fn message_with_thinking(
    view: JsonView<'_>,
    writer: &mut dyn Write,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<()> {
    let mut out = ObjectWriter::new(writer)?;
    view.for_each_entry(|key, value| {
        if key != "content" || !value.is_array()? {
            return out.field_with(key, |mut writer| value.write_canonical(&mut writer));
        }
        out.field_with(key, |mut writer| {
            writer.write_all(b"[")?;
            let mut first = true;
            for part in value.array()?.unwrap() {
                let part = part?;
                let thinking = part
                    .get("type")?
                    .map(|value| crate::affordance::input::word(value, &["thinking"]))
                    .transpose()?
                    .flatten()
                    == Some(0);
                let block = if thinking {
                    super::super::record::block_with(part, true, &mut |v| v.source_text())?
                } else {
                    None
                };
                let fate = block
                    .as_ref()
                    .map(|block| crate::emit::thinking::fate(block, Harness::OpenClaw, mode, resolver))
                    .transpose()?;
                if fate == Some(ThinkingFate::Drop) {
                    continue;
                }
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                if let Some(text) = block
                    .as_ref()
                    .zip(fate)
                    .and_then(|(block, fate)| crate::emit::thinking::text_with_fate(block, fate))
                {
                    let mut projected = ObjectWriter::new(writer)?;
                    projected.field("type", "text")?;
                    projected.field_with("text", |w| wire::text(w, &text, resolver))?;
                    projected.finish()?;
                } else {
                    part.write_canonical(&mut writer)?;
                }
            }
            writer.write_all(b"]")?;
            Ok(())
        })
    })?;
    out.finish()
}
