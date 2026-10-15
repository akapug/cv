//! Accept one source snapshot before retaining its lazy file bodies for native emission.
use super::*;
use crate::json::JsonSource;
use crate::source::{Probe, ReadSet};
use crate::stream::{Flow, MessageSink};
use std::collections::HashMap;

struct Collect {
    messages: Vec<crate::Message>,
    index: usize,
    start: usize,
    end: Option<usize>,
    primary: PathBuf,
    implicit: PathBuf,
    reads: ReadSet,
    sources: HashMap<PathBuf, JsonSource>,
    error: Option<anyhow::Error>,
}
impl Collect {
    fn capture(&mut self, path: &Path) -> Result<()> {
        let path = std::path::absolute(path)?;
        self.reads.record(&path);
        if !self.sources.contains_key(&path) {
            self.sources.insert(path.clone(), JsonSource::capture(&path)?);
        }
        Ok(())
    }
    fn note(&mut self, message: crate::Message) -> Result<()> {
        crate::source::record_message_reads(&message, &self.primary, &mut self.reads);
        let index = self.index;
        self.index += 1;
        if index < self.start || self.end.is_some_and(|end| index >= end) {
            return Ok(());
        }
        for block in &message.content {
            if let Some(crate::Text::Span(span)) = text(block) {
                let path = span.source.clone().unwrap_or_else(|| self.implicit.clone());
                self.capture(&path)?;
            }
        }
        self.messages.push(message);
        Ok(())
    }
}
impl MessageSink for Collect {
    fn message(&mut self, message: crate::Message) -> Flow {
        if let Err(error) = self.note(message) {
            self.error = Some(error);
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }
    fn meta(&mut self, metadata: &Session) {
        if let Some(path) = &metadata.source_path {
            self.implicit = path.clone();
        }
    }
    fn dependency(&mut self, path: &Path) {
        self.reads.record(path);
        // Absence/directory membership are read-set guards, not retained body files.
        if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            if let Err(error) = self.capture(path) {
                self.error = Some(error);
            }
        }
    }
}
fn text(block: &crate::Block) -> Option<&crate::Text> {
    match block {
        crate::Block::Text { text } | crate::Block::Thinking { text, .. } => Some(text),
        crate::Block::ToolResult { content, .. } => Some(content),
        _ => None,
    }
}
fn text_mut(block: &mut crate::Block) -> Option<&mut crate::Text> {
    match block {
        crate::Block::Text { text } | crate::Block::Thinking { text, .. } => Some(text),
        crate::Block::ToolResult { content, .. } => Some(content),
        _ => None,
    }
}
pub(super) fn parse(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    options: &ParseOptions,
) -> Result<Session> {
    Ok(selected(adapter, reference, options, 0, None)?.0)
}

pub(super) fn selected(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    options: &ParseOptions,
    start: usize,
    end: Option<usize>,
) -> Result<(Session, usize)> {
    let primary = adapter.resume_source(reference);
    let snapshot = Probe::default().snapshot_from(reference, &primary);
    let mut reads = snapshot
        .all_reads()
        .ok_or_else(|| anyhow::anyhow!("cannot capture source inventory"))?;
    reads.record(&primary);
    let mut sink = Collect {
        messages: Vec::new(),
        index: 0,
        start,
        end,
        primary: primary.clone(),
        implicit: reference.path.clone(),
        reads,
        sources: HashMap::new(),
        error: None,
    };
    if fs::metadata(&primary).is_ok_and(|metadata| metadata.is_file()) {
        sink.capture(&primary)?;
    }
    let read = crate::stream::stream_counted(adapter, reference, options, &mut sink, Default::default())?;
    let (mut session, counted) = (read.session, read.counted);
    if let Some(counted) = &counted {
        sink.reads.merge(&counted.dependencies);
    }
    let guard = snapshot.with_reads(&sink.reads);
    anyhow::ensure!(
        sink.reads.fresh(&mut Probe::default()) && crate::source::unchanged_from(reference, &primary, &guard),
        "source changed during port preparation"
    );
    let resolver = session.resolver();
    for message in &mut sink.messages {
        for block in &mut message.content {
            let Some(value) = text_mut(block) else { continue };
            let crate::Text::Span(span) = value else { continue };
            let span = span.clone();
            let path = std::path::absolute(
                span.source
                    .as_ref()
                    .or(session.source_path.as_ref())
                    .unwrap_or(&primary),
            )?;
            let source = sink
                .sources
                .get(&path)
                .ok_or_else(|| anyhow::anyhow!("uncaptured retained span source: {}", path.display()))?;
            source.check()?;
            let mut literal_backslash = false;
            if !span.escaped {
                value.try_for_each_chunk(&resolver, 64 * 1024, |chunk| {
                    literal_backslash |= chunk.contains('\\');
                    Ok(())
                })?;
            }
            let quoted = if !literal_backslash {
                span.offset
                    .checked_sub(1)
                    .zip(span.len.checked_add(2))
                    .filter(|(offset, len)| {
                        offset
                            .checked_add(*len)
                            .is_some_and(|end| fs::metadata(&path).is_ok_and(|metadata| end <= metadata.len()))
                    })
                    .map(|(offset, len)| {
                        source.read_range_value(offset, len, |view| {
                            if view.source_range()? == Some((offset, len)) {
                                view.source_text()
                            } else {
                                Ok(None)
                            }
                        })
                    })
            } else {
                None
            };
            let bound = match quoted {
                Some(Ok(Some(text))) => text,
                Some(Err(error)) if error.downcast_ref::<crate::json::InvalidJson>().is_none() => return Err(error),
                _ => crate::harness::store::text::append(source, &source.decoded_backing()?, |builder| {
                    value.try_for_each_chunk(&resolver, 64 * 1024, |chunk| builder.push(chunk))
                })?,
            };
            source.check()?;
            *value = bound;
        }
    }

    session.messages = sink.messages;
    if let Some(counted) = counted {
        counted.record_with_source(reference, &guard);
    }
    Ok((session, sink.index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Adapter;
    use crate::{Block, Harness, Message, Role, Text};
    fn assert_serialized_equal(actual: &Session, expected: &Session) -> Result<()> {
        let a = serde_json::to_vec(actual)?;
        let b = serde_json::to_vec(expected)?;
        if a != b {
            let offset = a
                .iter()
                .zip(&b)
                .position(|(a, b)| a != b)
                .unwrap_or(a.len().min(b.len()));
            let start = offset.saturating_sub(80);
            anyhow::bail!(
                "serialized IR differs at {offset}: actual={} expected={}",
                String::from_utf8_lossy(&a[start..a.len().min(offset + 160)]),
                String::from_utf8_lossy(&b[start..b.len().min(offset + 160)])
            );
        }
        Ok(())
    }

    #[test]
    fn complete_claude_preparation_matches_owned_fields_order_and_carriers() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-complete");
        let path = world.base.join("session.jsonl");
        let body = "escaped\n\" λ".repeat(12000);
        let rows = [
            serde_json::json!({"type":"user","uuid":"u","cwd":"/synthetic/complete","timestamp":"2026-10-12T12:00:00Z","message":{"role":"user","content":body}}),
            serde_json::json!({"type":"assistant","uuid":"synthetic","future_before":null,"isSidechain":false,"userType":"external","isApiErrorMessage":true,"errorDetails":{"small":"kept"},"requestId":"request","version":"v","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":body}],"future_message":17,"id":"provider-response-id","stop_reason":null,"stop_sequence":null,"stop_details":{"small":"kept"}}}),
            serde_json::json!({"type":"user","uuid":"compact","isCompactSummary":true,"message":{"role":"user","content":body}}),
            serde_json::json!({"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"thinking","thinking":body,"signature":"signed"},{"type":"tool_use","id":"c","name":"Read","input":{"blob":body}}]}}),
            serde_json::json!({"type":"user","uuid":"r","toolUseResult":{"stdout":"kept","agentId":"agent","status":"completed"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":body}]}}),
            serde_json::json!({"type":"future-meta","sessionId":"old","message":{"content":"verbatim carrier"},"unknown":{"kept":true}}),
        ];
        let mut wire = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
        wire.push_str(&format!(r#"{{"type":"user","uuid":"ordered","future_before":1,"message":{{"role":"user","content":{}}},"future_after_one":2,"future_after_two":3}}"#, serde_json::to_string(&body)?));
        wire.push('\n');
        wire.push_str(r#"{"type":"user","uuid":"old","uuid":"last","future":1,"future":2,"message":{"role":"user","content":"duplicate keys"}}"#);
        wire.push_str("\n{\"type\":\"user\",\"uuid\":\"invalid\",\"message\":{\"content\":\"must skip\"},\"ignored\":\"\\ud800\"}\n");
        fs::write(&path, &wire)?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        for (extra, usage_identity) in [(true, false), (false, false), (false, true)] {
            let owned_options = ParseOptions {
                extra,
                usage_identity,
                ..ParseOptions::complete()
            };
            let expected = crate::stream::collect_with(&adapter, &reference, &owned_options)?;
            assert!(expected
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .all(|block| text(block).is_none_or(|t| t.inline_str().is_some())));
            let checked_options = ParseOptions {
                spans: true,
                ..owned_options
            };
            struct Facts(crate::stream::CollectSink);
            impl MessageSink for Facts {
                fn extra_projection(&self) -> crate::stream::ExtraProjection {
                    crate::stream::ExtraProjection::Facts
                }
                fn message(&mut self, message: Message) -> Flow {
                    self.0.message(message)
                }
            }
            let mut facts = Facts(crate::stream::CollectSink::default());
            adapter.stream(&reference, &checked_options, &mut facts)?;
            let mut fact_session = Session::new("facts", Harness::Claude);
            fact_session.source_path = Some(path.clone());
            fact_session.messages = facts.0.messages;
            fact_session.materialize()?;
            let actual_bytes = serde_json::to_vec(&fact_session.messages)?;
            let expected_bytes = serde_json::to_vec(&expected.messages)?;
            anyhow::ensure!(
                actual_bytes == expected_bytes,
                "complete overrides Facts construction demand"
            );
            let actual = super::parse(&adapter, &reference, &checked_options)?;
            assert!(actual
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|block| matches!(text(block), Some(Text::Source(_)))));
            let mut materialized = actual.clone();
            materialized.materialize()?;
            assert_serialized_equal(&materialized, &expected)?;
            let options = EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                force: true, // this test emits the same pinned id again
                ..Default::default()
            };
            let (old, old_report) = super::super::emit_report(
                &expected,
                Harness::Claude,
                &world.base.join(format!("owned-{extra}")),
                &options,
            )?;
            let (new, new_report) = super::super::emit_report(
                &actual,
                Harness::Claude,
                &world.base.join(format!("checked-{extra}")),
                &options,
            )?;
            assert_eq!(fs::read(&new.path)?, fs::read(&old.path)?, "extra={extra}");
            assert_eq!(serde_json::to_value(new_report)?, serde_json::to_value(old_report)?);
            let before = fs::read(&new.path)?;
            let prepared = actual;
            fs::write(&path, b"{}")?;
            assert!(super::super::emit_report(
                &prepared,
                Harness::Claude,
                &world.base.join(format!("checked-{extra}")),
                &options
            )
            .is_err());
            assert_eq!(fs::read(&new.path)?, before);
            fs::write(&path, &wire)?;
        }
        Ok(())
    }

    #[test]
    fn complete_codex_preparation_matches_owned_acceptance_and_late_carriers() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-complete-codex");
        let path = world.base.join("rollout.jsonl");
        let body = "escaped\n\" λ".repeat(12000);
        let mut rows = vec![
            serde_json::json!({"type":"session_meta","timestamp":"2026-10-12T12:00:00Z","payload":{"id":"session","cwd":"/synthetic/complete","future":"kept"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":body}]}}),
        ];
        // The layout detector deliberately commits to no NL events after this gap.
        for i in 0..35 {
            rows.push(serde_json::json!({"type":"world_state","payload":{"i":i,"future":"kept"}}));
        }
        rows.extend([
            serde_json::json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":body}],"encrypted_content":"opaque"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c","name":"Read","arguments":serde_json::json!({"blob":body}).to_string()}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":body}}),
            serde_json::json!({"type":"event_msg","future_top":"kept","payload":{"type":"user_message","message":body,"future_payload":"kept"}}),
        ]);
        let mut wire = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
        wire.push_str("{\"type\":\"compacted\",\"payload\":{\"message\":\"must skip\",\"replacement_history\":[{\"ignored\":\"\\ud800\"}]}}\n");
        fs::write(&path, &wire)?;
        let reference = SessionRef::new("session", Harness::Codex, &path);
        let adapter = crate::harness::codex::Codex::detect();
        let expected = crate::stream::collect_with(&adapter, &reference, &ParseOptions::complete())?;
        let actual = super::super::parse_for_emit(&adapter, &reference, Harness::Codex)?;
        assert!(actual
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|block| matches!(text(block), Some(Text::Source(_)))));
        let mut materialized = actual.clone();
        materialized.materialize()?;
        assert_serialized_equal(&materialized, &expected)?;
        let options = EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        };
        let (old, old_report) =
            super::super::emit_report(&expected, Harness::Codex, &world.base.join("owned"), &options)?;
        let (new, new_report) =
            super::super::emit_report(&actual, Harness::Codex, &world.base.join("checked"), &options)?;
        assert_eq!(fs::read(&new.path)?, fs::read(&old.path)?);
        assert_eq!(serde_json::to_value(new_report)?, serde_json::to_value(old_report)?);
        let before = fs::read(&new.path)?;
        fs::write(&path, b"{}")?;
        assert!(super::super::emit_report(&actual, Harness::Codex, &world.base.join("checked"), &options).is_err());
        assert_eq!(fs::read(&new.path)?, before);
        Ok(())
    }

    #[test]
    fn complete_gemini_preparation_matches_owned_recordings_replay_and_source_errors() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-complete-gemini");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats)?;
        let body = "escaped\n\" λ🌙".repeat(6000);
        let meta = serde_json::json!({"sessionId":"source","projectHash":"project","startTime":"2026-10-12T12:00:00Z","lastUpdated":"2026-10-12T12:00:00Z","summary":"complete title","directories":["/synthetic/complete-gemini"],"kind":"subagent","future":{"ignored":true}});
        let rows = vec![
            serde_json::json!({"type":"user","id":"u","timestamp":"2026-10-12T12:00:00Z","content":[{"text":"small prompt"},{"inlineData":{"mimeType":"image/png","data":"small"}},{"fileData":{"mimeType":"text/plain","fileUri":"file:///synthetic/input"}}]}),
            serde_json::json!({"type":"gemini","id":"a","timestamp":"2026-10-12T12:00:00Z","model":"first-model","tokens":{"input":11,"output":3,"cached":2},"content":[{"text":body},{"text":"duplicate thought","thought":true},{"functionCall":{"id":"call","name":"Read","args":{"duplicate":"suppressed"}}}],"thoughts":[{"subject":"subject","description":body}],"toolCalls":[{"id":"call","name":"Read","agentId":"child","args":{"blob":body},"status":"success","result":[{"text":body},{"functionResponse":{"response":{"output":body}}}]},{"id":"object","name":"custom","args":null,"status":"error","result":{"z":body,"a":1}}],"future":{"ignored":true}}),
            serde_json::json!({"type":"user","id":"result-copy","content":[{"functionResponse":{"id":"call","name":"Read","response":{"output":body}}}]}),
            serde_json::json!({"type":"warning","id":"warning","content":body}),
            serde_json::json!({"type":"future-record","id":"future","content":"unknown record remains a native user turn","unknown":{"not_a_complete_carrier":true}}),
            serde_json::json!({"type":"gemini","id":"other-model","model":"second-model","content":"final answer"}),
        ];
        let normalized = |path: &Path| -> Result<Vec<u8>> {
            let mut raw = fs::read(path)?;
            let value: Value = serde_json::from_slice(&raw)?;
            for message in value["messages"].as_array().unwrap() {
                let id = message["id"].as_str().unwrap();
                anyhow::ensure!(uuid::Uuid::parse_str(id).is_ok(), "expected generated message UUID");
                let text = String::from_utf8(raw)?;
                raw = text.replace(id, "GENERATED-MESSAGE-ID").into_bytes();
            }
            Ok(raw)
        };
        let adapter = crate::harness::gemini::Gemini::detect();
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-modern.jsonl"
            } else {
                "session-legacy.json"
            });
            let wire = if modern {
                let discarded = serde_json::json!({"type":"user","id":"discard","content":"removed by rewind"});
                format!("{meta}\n{{\"type\":\"user\",\"id\":\"old\",\"content\":\"replaced by set\"}}\n{}\n{discarded}\n{{\"$rewindTo\":\"discard\"}}\n{{\"$set\":{{\"summary\":\"patched title\",\"kind\":\"subagent\"}}}}\n{{\"type\":\"user\",\"id\":\"duplicate-old\",\"id\":\"duplicate-last\",\"content\":\"duplicate winner\"}}\n{{\"type\":\"user\",\"id\":\"invalid\",\"content\":\"must skip\",\"ignored\":\"\\ud800\"}}\n", serde_json::json!({"$set":{"messages":rows}}))
            } else {
                let mut object = meta.as_object().unwrap().clone();
                object.insert("messages".into(), Value::Array(rows.clone()));
                Value::Object(object).to_string()
            };
            fs::write(&path, &wire)?;
            let reference = SessionRef::new("source", Harness::Gemini, &path);
            let expected = crate::stream::collect_with(&adapter, &reference, &ParseOptions::complete())?;
            assert!(expected
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .all(|b| !matches!(text(b), Some(Text::Span(_) | Text::Source(_)))));
            let actual = super::super::parse_for_emit(&adapter, &reference, Harness::Gemini)?;
            assert!(actual
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(text(b), Some(Text::Source(_)))));
            let mut materialized = actual.clone();
            materialized.materialize()?;
            assert_serialized_equal(&materialized, &expected)?;
            for extra in [false, true] {
                let options = ParseOptions {
                    spans: true,
                    extra,
                    usage_identity: !extra,
                    ..ParseOptions::complete()
                };
                let mut custom = crate::stream::collect_with(&adapter, &reference, &options)?;
                custom.materialize()?;
                assert_serialized_equal(&custom, &expected)?;
            }
            let options = EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                ..Default::default()
            };
            let owned_dir = world.base.join(if modern { "owned-modern" } else { "owned-legacy" });
            let checked_dir = world
                .base
                .join(if modern { "checked-modern" } else { "checked-legacy" });
            let (old, old_report) = super::super::emit_report(&expected, Harness::Gemini, &owned_dir, &options)?;
            let (new, new_report) = super::super::emit_report(&actual, Harness::Gemini, &checked_dir, &options)?;
            assert_eq!(normalized(&new.path)?, normalized(&old.path)?);
            assert_eq!(serde_json::to_value(new_report)?, serde_json::to_value(old_report)?);
            let before = fs::read(&new.path)?;
            fs::write(&path, b"{}")?;
            assert!(super::super::emit_report(&actual, Harness::Gemini, &checked_dir, &options).is_err());
            assert!(super::super::emit(&actual, Harness::Gemini, &checked_dir, &options).is_err());
            assert_eq!(fs::read(&new.path)?, before);
        }
        Ok(())
    }

    #[test]
    fn actual_claude_preparation_retains_original_body_identity() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-claude");
        let path = world.base.join("session.jsonl");
        let body = "plain retained body λ".repeat(512);
        fs::write(
            &path,
            serde_json::to_vec(
                &serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":[{"type":"text","text":body}]}}),
            )?,
        )?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let session =
            super::super::parse_for_emit(&crate::harness::claude::Claude::detect(), &reference, Harness::Codex)?;
        assert!(matches!(
            &session.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let output = world.base.join("output");
        let options = EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        };
        let result = emit(&session, Harness::Codex, &output, &options)?;
        let bytes = fs::read(&result.path)?;
        let mut changed = fs::read(&path)?;
        let at = changed.windows(5).position(|bytes| bytes == b"plain").unwrap();
        changed[at] = b'P';
        fs::write(&path, changed)?;
        assert!(emit(&session, Harness::Codex, &output, &options).is_err());
        assert_eq!(fs::read(&result.path)?, bytes);
        Ok(())
    }
    #[test]
    fn selected_preparation_keeps_only_window_and_original_guard() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare");
        let path = world.base.join("session.jsonl");
        let mut bytes = Vec::new();
        for index in 0..12 {
            let body = if index == 5 {
                "selected λ".repeat(1024)
            } else {
                "discarded".repeat(128 * 1024)
            };
            serde_json::to_writer(
                &mut bytes,
                &serde_json::json!({
                    "type":"user", "uuid":format!("u{index}"), "cwd":"/tmp/selected", "timestamp":"2025-01-01T00:00:00Z",
                    "message":{"role":"user","content":[{"type":"text","text":body}]}
                }),
            )?;
            bytes.push(b'\n');
        }
        fs::write(&path, bytes)?;
        let reference = SessionRef::new("session", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        let mut old = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full())?;
        old.materialize()?;
        for (start, end) in [(5, Some(6)), (0, Some(0)), (8, Some(2)), (30, None), (11, None)] {
            let (mut selected, total) = super::super::prepare_selected(&adapter, &reference, start, end)?;
            assert_eq!(total, 12);
            let start = start.min(total);
            let end = end.unwrap_or(total).min(total).max(start);
            assert_eq!(selected.messages.len(), end - start);
            assert_eq!(selected.cwd, old.cwd);
            selected.materialize()?;
            assert_eq!(
                serde_json::to_value(&selected.messages)?,
                serde_json::to_value(&old.messages[start..end])?
            );
        }
        let (selected, _) = super::super::prepare_selected(&adapter, &reference, 5, Some(6))?;
        assert!(matches!(
            &selected.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let mut builder = crate::loom::Composition::default();
        builder.push(selected.clone(), 12, 5, Some(6));
        let actual = builder.finish(Some("00000000-0000-0000-0000-000000000002".into()), Harness::Claude);
        let expected = crate::loom::splice(
            &[crate::loom::Span {
                source: &old,
                start: 5,
                end: Some(6),
            }],
            Some(actual.id.clone()),
            Harness::Claude,
        );
        let mut owned_actual = actual.clone();
        owned_actual.materialize()?;
        assert_eq!(serde_json::to_value(&owned_actual)?, serde_json::to_value(&expected)?);
        assert_eq!(
            crate::render::to_markdown(&owned_actual)?,
            crate::render::to_markdown(&expected)?
        );
        for target in [Harness::Claude, Harness::Codex] {
            let options = EmitOptions {
                new_id: Some(actual.id.clone()),
                ..Default::default()
            };
            let old_result = emit(&expected, target, &world.base.join(format!("old-{target}")), &options)?;
            let new_result = emit(&actual, target, &world.base.join(format!("new-{target}")), &options)?;
            assert!(
                fs::read(old_result.path)? == fs::read(new_result.path)?,
                "native {target} output"
            );
        }
        // Retained descriptors continue to reject a changed source after preparation returns.
        fs::write(&path, b"changed")?;
        let mut selected = selected;
        assert!(selected.materialize().is_err());
        Ok(())
    }

    struct Sidecar {
        path: PathBuf,
        mutate: bool,
        range: Option<(u64, u64)>,
    }
    impl crate::harness::Adapter for Sidecar {
        fn harness(&self) -> Harness {
            Harness::Claude
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<Vec<SessionRef>> {
            Ok(Vec::new())
        }
        fn stream(&self, reference: &SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
            let mut session = Session::new(&reference.id, Harness::Claude);
            session.source_path = Some(reference.path.clone());
            sink.meta(&session);
            sink.dependency(&self.path);
            let len = fs::metadata(&self.path)?.len();
            let (offset, len) = self.range.unwrap_or((1, len - 1));
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text {
                text: Text::Span(crate::lazy::Span {
                    source: Some(self.path.clone()),
                    offset,
                    len,
                    escaped: false,
                }),
            });
            sink.try_message(message)?;
            if self.mutate {
                fs::write(&self.path, b"changed after source callback")?;
            }
            Ok(session)
        }
    }
    #[test]
    fn literal_sidecar_spans_are_bounded_snapshots_with_original_identity() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-sidecar");
        let primary = world.base.join("session.jsonl");
        let sidecar = world.base.join("body.txt");
        fs::write(&primary, b"{}")?;
        let body = "literal \\n body λ".repeat(1024);
        fs::write(&sidecar, format!("!{body}"))?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar.clone(),
            mutate: false,
            range: None,
        };
        let session = super::super::parse_for_emit(&adapter, &reference, Harness::Codex)?;
        assert!(matches!(
            &session.messages[0].content[0],
            Block::Text { text: Text::Source(_) }
        ));
        let materialized = super::super::materialized_session(&session)?;
        assert_eq!(materialized.messages[0].text().as_deref(), Some(body.as_str()));
        fs::write(&sidecar, b"!changed")?;
        assert!(emit(
            &session,
            Harness::Codex,
            &world.base.join("output"),
            &EmitOptions::default()
        )
        .is_err());
        fs::write(&sidecar, format!("!{body}"))?;
        assert!(super::super::parse_for_emit(
            &Sidecar {
                path: sidecar,
                mutate: true,
                range: None,
            },
            &reference,
            Harness::Codex
        )
        .is_err());
        Ok(())
    }
    #[test]
    fn selected_opensession_defaults_to_underlying_harness_and_provenance() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-opensession");
        let path = world.base.join("source.opensession.json");
        let mut original = Session::new("wrapped-codex", Harness::Codex);
        original.cwd = Some("/tmp/wrapped".into());
        for text in ["head", "selected", "tail"] {
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.into() });
            original.messages.push(message);
        }
        fs::write(
            &path,
            serde_json::to_vec(&crate::harness::opensession::document(&original))?,
        )?;
        let reference = SessionRef::new("wrapped-codex", Harness::OpenSession, &path);
        let adapter = crate::harness::opensession::OpenSession;
        let mut legacy = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full())?;
        legacy.materialize()?;
        let (selected, total) = super::super::prepare_selected(&adapter, &reference, 1, Some(2))?;
        assert_eq!(selected.harness, Harness::Codex);
        let mut builder = crate::loom::Composition::default();
        builder.push(selected, total, 1, Some(2));
        let default_target = builder.first_harness().unwrap();
        assert_eq!(default_target, legacy.harness);
        let mut actual = builder.finish(Some("fixed".into()), default_target);
        actual.materialize()?;
        let expected = crate::loom::splice(
            &[crate::loom::Span {
                source: &legacy,
                start: 1,
                end: Some(2),
            }],
            Some("fixed".into()),
            legacy.harness,
        );
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        Ok(())
    }

    #[test]
    fn selected_rich_messages_preserve_full_fidelity_and_eof_defaults() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-rich");
        let path = world.base.join("rich.jsonl");
        let rows = [
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":"head"}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","timestamp":"2025-01-01T00:00:00Z","message":{"role":"assistant","model":"model","usage":{"input_tokens":20,"output_tokens":10},"content":[{"type":"thinking","thinking":"reason","signature":"signature"},{"type":"tool_use","id":"tool","name":"Read","input":{"path":"/tmp/input","large":"x".repeat(8192)}}]}}),
            serde_json::json!({"type":"user","uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool","content":[{"type":"text","text":"result λ".repeat(1024)}],"is_error":false}]},"toolUseResult":{"rich":"metadata"}}),
            serde_json::json!({"type":"user","uuid":"tail","cwd":"/tmp/eof-default","message":{"role":"user","content":"tail"}}),
        ];
        let mut bytes = Vec::new();
        for row in rows {
            serde_json::to_writer(&mut bytes, &row)?;
            bytes.push(b'\n');
        }
        fs::write(&path, bytes)?;
        let reference = SessionRef::new("rich", Harness::Claude, &path);
        let adapter = crate::harness::claude::Claude::detect();
        let mut old = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full())?;
        old.materialize()?;
        let mut builder = crate::loom::Composition::default();
        for (start, end) in [(1, Some(3)), (2, Some(3))] {
            let (source, total) = super::super::prepare_selected(&adapter, &reference, start, end)?;
            assert_eq!(total, old.messages.len());
            assert_eq!(source.cwd, old.cwd);
            builder.push(source, total, start, end);
        }
        let mut actual = builder.finish(Some("fixed".into()), Harness::Claude);
        actual.materialize()?;
        let expected = crate::loom::splice(
            &[
                crate::loom::Span {
                    source: &old,
                    start: 1,
                    end: Some(3),
                },
                crate::loom::Span {
                    source: &old,
                    start: 2,
                    end: Some(3),
                },
            ],
            Some("fixed".into()),
            Harness::Claude,
        );
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        Ok(())
    }

    #[test]
    fn selected_preparation_checks_discarded_dependencies_and_failure() -> Result<()> {
        let world = cv_testkit::World::new("selected-prepare-dependency");
        let primary = world.base.join("primary");
        let sidecar = world.base.join("sidecar");
        fs::write(&primary, b"{}")?;
        fs::write(&sidecar, b"!discarded")?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar,
            mutate: true,
            range: None,
        };
        // Even an empty selection must reject metadata/dependency mutation during the EOF pass.
        assert!(super::super::prepare_selected(&adapter, &reference, 5, None).is_err());
        Ok(())
    }

    #[test]
    fn literal_quoted_text_does_not_become_the_enclosing_json_value() -> Result<()> {
        let world = cv_testkit::World::new("port-prepare-literal-quotes");
        let primary = world.base.join("session.jsonl");
        let sidecar = world.base.join("body.txt");
        fs::write(&primary, b"{}")?;
        fs::write(&sidecar, br#"[ "x" ]"#)?;
        let reference = SessionRef::new("session", Harness::Claude, &primary);
        let adapter = Sidecar {
            path: sidecar,
            mutate: false,
            range: Some((2, 3)),
        };
        let session = super::super::parse_for_emit(&adapter, &reference, Harness::Codex)?;
        let owned = super::super::materialized_session(&session)?;
        assert_eq!(owned.messages[0].text().as_deref(), Some("\"x\""));
        Ok(())
    }
}

#[cfg(test)]
mod same_target;
