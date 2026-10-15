//! Checked complete records retain the public `extra._record` contract.
use super::Block;
use super::{Harness, Message};
use crate::{harness::wire::write::ObjectWriter, Json};
use anyhow::Result;
use serde::{ser::SerializeMap, Serialize, Serializer};
use serde_json::{Map, Value};
use std::io::Write;

#[derive(Debug, Clone)]
pub struct CompleteRecord {
    #[cfg_attr(not(any(feature = "sqlite", test)), allow(dead_code))]
    pub(crate) harness: Harness,
    pub(crate) value: Json,
    pub(crate) identity: Map<String, Value>,
    // Ordinary source descriptors clone cheaply. Uncommon owned metadata keeps its owned cost.
    pub(crate) projection: Vec<Message>,
}
impl CompleteRecord {
    /// Retain a complete record solely for its public serialization contract.
    pub(crate) fn attach(messages: &mut [Message], harness: Harness, value: Json, identity: Map<String, Value>) {
        Self::attach_inner(messages, harness, value, identity, false);
    }
    /// Retain cheap checked projection descriptors for unchanged native replay.
    pub(crate) fn attach_replay(messages: &mut [Message], harness: Harness, value: Json, identity: Map<String, Value>) {
        Self::attach_inner(messages, harness, value, identity, true);
    }
    fn attach_inner(
        messages: &mut [Message],
        harness: Harness,
        value: Json,
        identity: Map<String, Value>,
        replay: bool,
    ) {
        if messages.is_empty() {
            return;
        }
        let projection = if replay && matches!(value, Json::Source(_)) {
            messages
                .iter()
                .map(|message| Message {
                    id: message.id.clone(),
                    parent_id: message.parent_id.clone(),
                    role: message.role,
                    kind: message.kind,
                    origin: message.origin,
                    timestamp: message.timestamp,
                    model: message.model.clone(),
                    content: message.content.clone(),
                    usage: message.usage.clone(),
                    extra: message
                        .extra
                        .iter()
                        .filter(|(key, _)| key.as_str() != crate::harness::claude::CARRIER_KEY)
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                    complete_record: None,
                })
                .collect()
        } else {
            Vec::new()
        };
        messages[0].extra.remove(crate::harness::claude::CARRIER_KEY);
        messages[0].complete_record = Some(std::sync::Arc::new(Self {
            harness,
            value,
            identity,
            projection,
        }));
    }
    /// Full reads own raw payloads; keep only the captured envelopes needed to reject
    /// edited metadata when a native codec reconstructs that record's blocks later.
    pub(crate) fn materialize(&mut self) -> Result<()> {
        self.value.make_owned()?;
        for message in &mut self.projection {
            message.content.clear();
        }
        Ok(())
    }
    pub(crate) fn unchanged(&self, harness: Harness, messages: &[Message]) -> bool {
        self.harness == harness
            && matches!(self.value, Json::Source(_))
            && !self.projection.is_empty()
            && messages.len() >= self.projection.len()
            && self
                .projection
                .iter()
                .zip(messages)
                .all(|(original, current)| same_message(original, current))
    }
    #[cfg(any(feature = "sqlite", test))]
    pub(crate) fn text_overlay(&self, harness: Harness, messages: &[Message]) -> Result<bool> {
        if self.harness != harness || self.projection.is_empty() || messages.len() < self.projection.len() {
            return Ok(false);
        }
        let ty = self.identity.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(ty, "user")
            || !same_except_first_text(&self.projection[0], &messages[0])
            || !self
                .projection
                .iter()
                .skip(1)
                .zip(messages.iter().skip(1))
                .all(|(a, b)| same_message(a, b))
        {
            return Ok(false);
        }
        self.value.with_view(|view| match view.get("text")? {
            Some(value) if value.is_string()? => match value.source_range()? {
                Some((_, len)) => Ok(len > 2),
                None => Ok(value.resolve()?.as_str().is_some_and(|text| !text.is_empty())),
            },
            _ => Ok(false),
        })
    }
    pub(crate) fn write(&self, writer: &mut dyn Write, include_identity: bool) -> Result<()> {
        self.value.with_view(|view| {
            let mut out = ObjectWriter::new(writer)?;
            view.for_each_entry(|key, value| {
                if !self.identity.contains_key(key) && (include_identity || !matches!(key, "id" | "type" | "sessionID"))
                {
                    out.field_with(key, |mut w| value.write_canonical(&mut w))?;
                }
                Ok(())
            })?;
            if include_identity {
                for (key, value) in &self.identity {
                    out.field(key, value)?;
                }
            }
            out.finish()
        })
    }
}
impl Serialize for CompleteRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let value = self.value.resolve().map_err(serde::ser::Error::custom)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::ser::Error::custom("complete record is not an object"))?;
        let mut out = serializer.serialize_map(None)?;
        for (key, value) in object {
            if !self.identity.contains_key(key) {
                out.serialize_entry(key, value)?;
            }
        }
        for (key, value) in &self.identity {
            out.serialize_entry(key, value)?;
        }
        out.end()
    }
}
pub(crate) struct Extra<'a>(pub &'a Message);
impl Serialize for Extra<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_map(None)?;
        for (key, value) in &self.0.extra {
            out.serialize_entry(key, value)?;
        }
        // An explicit owned edit wins over the retained checked record.
        if !self.0.extra.contains_key(crate::harness::claude::CARRIER_KEY) {
            if let Some(record) = &self.0.complete_record {
                out.serialize_entry(crate::harness::claude::CARRIER_KEY, record.as_ref())?;
            }
        }
        out.end()
    }
}
pub(crate) fn write_extra(message: &Message, writer: &mut dyn Write) -> Result<()> {
    let mut out = ObjectWriter::new(writer)?;
    for (key, value) in &message.extra {
        out.field(key, value)?;
    }
    if !message.extra.contains_key(crate::harness::claude::CARRIER_KEY) {
        if let Some(record) = &message.complete_record {
            out.field_with(crate::harness::claude::CARRIER_KEY, |w| record.write(w, true))?;
        }
    }
    out.finish()
}
fn same_json(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Owned(a), Json::Owned(b)) => a == b,
        (Json::Source(a), Json::Source(b)) => a.same_range(b),
        _ => false,
    }
}
fn same_block(a: &Block, b: &Block) -> bool {
    match (a, b) {
        (Block::Text { text: a }, Block::Text { text: b }) => a == b,
        (
            Block::Thinking {
                text: a,
                signature: asig,
                encrypted: ae,
                redacted: ar,
            },
            Block::Thinking {
                text: b,
                signature: bsig,
                encrypted: be,
                redacted: br,
            },
        ) => a == b && asig == bsig && ae == be && ar == br,
        (
            Block::ToolUse {
                id: a,
                name: an,
                input: ai,
                namespace: ans,
            },
            Block::ToolUse {
                id: b,
                name: bn,
                input: bi,
                namespace: bns,
            },
        ) => a == b && an == bn && ans == bns && same_json(ai, bi),
        (
            Block::ToolResult {
                tool_use_id: a,
                content: ac,
                is_error: ae,
                tool_name: an,
                status: as_,
                details: ad,
            },
            Block::ToolResult {
                tool_use_id: b,
                content: bc,
                is_error: be,
                tool_name: bn,
                status: bs,
                details: bd,
            },
        ) => a == b && ac == bc && ae == be && an == bn && as_ == bs && ad == bd,
        (
            Block::Image {
                media_type: a,
                data_ref: ar,
            },
            Block::Image {
                media_type: b,
                data_ref: br,
            },
        ) => a == b && ar == br,
        (
            Block::File {
                mime: a,
                path: ap,
                source: as_,
            },
            Block::File {
                mime: b,
                path: bp,
                source: bs,
            },
        ) => a == b && ap == bp && as_ == bs,
        _ => false,
    }
}
fn same_usage(a: &Option<super::Usage>, b: &Option<super::Usage>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.input_tokens == b.input_tokens
                && a.output_tokens == b.output_tokens
                && a.cache_read_tokens == b.cache_read_tokens
                && a.cache_creation_tokens == b.cache_creation_tokens
                && a.reasoning_tokens == b.reasoning_tokens
                && match (a.cost_usd, b.cost_usd) {
                    // Preserve Value serialization's null normalization for nonfinite costs;
                    // an absent cost is omitted and therefore remains distinct from null.
                    (Some(a), Some(b)) => (a == b) || (!a.is_finite() && !b.is_finite()),
                    (None, None) => true,
                    _ => false,
                }
        }
        _ => false,
    }
}
pub(crate) fn same_envelope(a: &Message, b: &Message) -> bool {
    a.id == b.id
        && a.parent_id == b.parent_id
        && a.role == b.role
        && a.kind == b.kind
        && a.origin == b.origin
        && a.timestamp == b.timestamp
        && a.model == b.model
        && same_usage(&a.usage, &b.usage)
        && a.extra
            .iter()
            .filter(|(key, _)| key.as_str() != crate::harness::claude::CARRIER_KEY)
            .eq(b
                .extra
                .iter()
                .filter(|(key, _)| key.as_str() != crate::harness::claude::CARRIER_KEY))
}

pub(crate) fn same_message(a: &Message, b: &Message) -> bool {
    same_envelope(a, b)
        && a.content.len() == b.content.len()
        && a.content.iter().zip(&b.content).all(|(a, b)| same_block(a, b))
}
#[cfg(any(feature = "sqlite", test))]
pub(crate) fn same_except_first_text(a: &Message, b: &Message) -> bool {
    same_envelope(a, b)
        && a.content.len() == b.content.len()
        && matches!(
            (a.content.first(), b.content.first()),
            (Some(Block::Text { .. }), Some(Block::Text { .. }))
        )
        && a.content
            .iter()
            .skip(1)
            .zip(b.content.iter().skip(1))
            .all(|(a, b)| same_block(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialized_empty_replay_never_looks_like_checked_unchanged() -> Result<()> {
        let world = crate::test_world("record-empty-replay");
        let path = world.base.join("empty.json");
        std::fs::write(&path, "{}")?;
        let mut messages = vec![Message::new(super::super::Role::User)];
        CompleteRecord::attach_replay(&mut messages, Harness::OpenClaw, Json::source(&path, 0, 2)?, Map::new());
        let mut record = messages[0].complete_record.as_ref().unwrap().as_ref().clone();
        assert!(record.unchanged(Harness::OpenClaw, &messages));
        record.materialize()?;
        assert!(!record.unchanged(Harness::OpenClaw, &messages));
        assert!(!record.text_overlay(Harness::OpenClaw, &messages)?);
        assert_eq!(record.projection.len(), 1);
        assert!(record.projection[0].content.is_empty());
        Ok(())
    }

    #[test]
    fn replay_usage_comparison_matches_serialized_value_oracle() {
        let mut usages = vec![None];
        for cost in [
            None,
            Some(0.0),
            Some(-0.0),
            Some(1.25),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ] {
            let usage = super::super::Usage {
                cost_usd: cost,
                ..Default::default()
            };
            usages.push(Some(usage.clone()));
            usages.push(Some(super::super::Usage {
                input_tokens: Some(0),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                output_tokens: Some(u64::MAX),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                cache_read_tokens: Some(1),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                cache_creation_tokens: Some(2),
                ..usage.clone()
            }));
            usages.push(Some(super::super::Usage {
                reasoning_tokens: Some(3),
                ..usage
            }));
        }
        for a in &usages {
            for b in &usages {
                assert_eq!(
                    same_usage(a, b),
                    serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap(),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }
}
