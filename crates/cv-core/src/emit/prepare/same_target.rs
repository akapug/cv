//! Same-target preparation is compared with independent owned complete collection.
use super::*;
use crate::{harness::Adapter, ir::MessageKind, Harness};
use serde_json::json;
use std::collections::BTreeMap;

fn files(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else {
                out.insert(path.strip_prefix(root)?.to_path_buf(), fs::read(path)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}
fn has_sources(session: &Session) -> bool {
    session.messages.iter().flat_map(|m| &m.content).any(|block| {
        block.text().is_some_and(|text| matches!(text, crate::Text::Source(_)))
            || matches!(
                block,
                Block::ToolUse {
                    input: crate::Json::Source(_),
                    ..
                }
            )
    })
}
/// Wire guarantees only one target's preparation can make: the shared walker pins what every
/// target must match, this hook pins what an individual one promises.
fn check_target(target: Harness, prepared: &Session, expected: &Session) -> Result<()> {
    match target {
        Harness::LmStudio => {
            let branches = prepared
                .messages
                .iter()
                .filter(|m| m.kind == MessageKind::Branch)
                .collect::<Vec<_>>();
            assert_eq!(branches.len(), 2);
            assert_eq!(
                branches
                    .iter()
                    .map(|m| m.harness_extra(target).unwrap()["variant"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                vec![0, 2]
            );
        }
        Harness::Cline | Harness::Roo => {
            assert_eq!(
                prepared
                    .messages
                    .iter()
                    .filter(|m| m.kind == MessageKind::Carrier)
                    .count(),
                expected
                    .messages
                    .iter()
                    .filter(|m| m.kind == MessageKind::Carrier)
                    .count()
            );
        }
        _ => {}
    }
    Ok(())
}

fn check(adapter: &dyn Adapter, reference: &SessionRef, target: Harness, output: &Path, source: &Path) -> Result<()> {
    let expected = crate::stream::collect_with(adapter, reference, &Demand::COMPLETE)?;
    assert!(!has_sources(&expected), "public complete must remain owned");
    let prepared = crate::emit::parse_for_emit(adapter, reference, target)?;
    assert!(has_sources(&prepared), "same-target preparation retained no sources");
    let mut resolved = prepared.clone();
    resolved.materialize()?;
    let old = serde_json::to_vec(&expected)?;
    let new = serde_json::to_vec(&resolved)?;
    assert!(
        old == new,
        "complete serialized IR mismatch at {:?}",
        old.iter().zip(&new).position(|(a, b)| a != b)
    );
    check_target(target, &prepared, &expected)?;
    for mode in crate::emit::ThinkingMode::ALL {
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            thinking: mode,
            ..Default::default()
        };
        let old_dir = output.join(format!("old-{mode:?}"));
        let new_dir = output.join(format!("new-{mode:?}"));
        let (_, old_report) = crate::emit::emit_report(&expected, target, &old_dir, &opts)?;
        let (emitted, new_report) = crate::emit::emit_report(&prepared, target, &new_dir, &opts)?;
        assert_eq!(serde_json::to_value(old_report)?, serde_json::to_value(new_report)?);
        assert!(
            files(&old_dir)? == files(&new_dir)?,
            "wire mismatch {target:?}/{mode:?}"
        );
        assert!(emitted.path.exists());
    }
    // Mutation comes after candidate preparation and successful publication, not before capture.
    let opts = crate::emit::EmitOptions {
        new_id: Some("fixed".into()),
        ..Default::default()
    };
    let destination = output.join("mutation");
    crate::emit::emit_report(&prepared, target, &destination, &opts)?;
    let before = files(&destination)?;
    fs::write(source, b"changed after preparation")?;
    assert!(crate::emit::emit_report(&prepared, target, &destination, &opts).is_err());
    assert!(files(&destination)? == before, "source failure changed published files");
    Ok(())
}

#[test]
fn same_target_complete_task_dirs_preserve_owned_ir_wire_and_source_errors() -> Result<()> {
    let world = crate::test_world("same-target-task-dirs");
    let body = "ordinary \\ λ\n".repeat(12000);
    for target in [Harness::Cline, Harness::Roo] {
        let root = world.base.join(target.as_str());
        fs::create_dir_all(&root)?;
        let source = root.join("api_conversation_history.json");
        fs::write(
            &source,
            serde_json::to_vec(&json!([
                {"role":"user","content":[{"type":"text","text":body}]},
                {"role":"assistant","content":[{"type":"thinking","thinking":body,"signature":"signature"},{"type":"tool_use","id":"call","name":"Read","input":{"text":body}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":body}]},
                {"role":"user","content":"summary","isSummary":true,"condenseId":"boundary"},
                {"role":"future","content":"unknown carrier candidate","future":{"kept":true}}
            ]))?,
        )?;
        fs::write(
            root.join("task_metadata.json"),
            br#"{"cwd":"/synthetic/same-target","future":true}"#,
        )?;
        fs::write(
            root.join("ui_messages.json"),
            br#"[{"ts":1791892800000,"text":"task"}]"#,
        )?;
        let reference = SessionRef::new("synthetic-source", target, &root);
        let adapter = crate::harness::for_harness(target).unwrap();
        check(
            adapter.as_ref(),
            &reference,
            target,
            &world.base.join(format!("output-{}", target.as_str())),
            &source,
        )?;
    }
    Ok(())
}

#[test]
fn same_target_complete_sdk_preserves_owned_ir_wire_and_source_errors() -> Result<()> {
    let world = crate::test_world("same-target-sdk");
    let source = world.base.join("synthetic-source.messages.json");
    let body = "sdk text λ\n".repeat(12000);
    fs::write(
        &source,
        serde_json::to_vec(
            &json!({"version":1,"agent":"lead","sessionId":"synthetic-source","task_type":"task","origin":{"future":true},"system_prompt":"system","messages":[
                {"role":"user","id":"u","ts":1791892800000_i64,"content":body},
                {"role":"assistant","id":"a","modelInfo":{"id":"model","provider":"test"},"metrics":{"inputTokens":9,"outputTokens":3},"content":[{"type":"thinking","thinking":body},{"type":"tool_use","id":"call","name":"Read","input":{"body":body}}]},
                {"role":"user","id":"r","content":[{"type":"tool_result","tool_use_id":"call","content":body}]},
                {"role":"assistant","id":"error","content":"display error","metadata":{"displayOnly":true,"displayRole":"error"}}
            ]}),
        )?,
    )?;
    fs::write(
        world.base.join("synthetic-source.json"),
        br#"{"cwd":"/synthetic/sdk","model":"model","metadata":{"title":"explicit"},"status":"completed"}"#,
    )?;
    let reference = SessionRef::new("synthetic-source", Harness::Cline, &source);
    check(
        &crate::harness::cline::Cline::detect(),
        &reference,
        Harness::Cline,
        &world.base.join("output"),
        &source,
    )
}

#[test]
fn same_target_complete_continue_preserves_owned_ir_wire_and_source_errors() -> Result<()> {
    let world = crate::test_world("same-target-continue");
    let body = "continue λ\n".repeat(12000);
    let source = world.base.join("session.json");
    fs::write(
        &source,
        serde_json::to_vec(
            &json!({"sessionId":"synthetic-source","title":"explicit","workspaceDirectory":"/synthetic/continue","history":[
                {"message":{"role":"system","content":"system"},"contextItems":[]},
                {"message":{"role":"user","content":[{"type":"text","text":body}]},"contextItems":[{"name":"input","uri":{"type":"file","value":"/synthetic/input.txt"}}]},
                {"message":{"role":"assistant","content":body,"toolCalls":[{"id":"call","type":"function","function":{"name":"Read","arguments":serde_json::to_string(&json!({"body":body}))?}}]},"promptLogs":[{"modelTitle":"model"}]},
                {"message":{"role":"tool","toolCallId":"call","content":body},"contextItems":[]}
            ]}),
        )?,
    )?;
    let reference = SessionRef::new("synthetic-source", Harness::Continue, &source);
    check(
        &crate::harness::continuedev::Continue::detect(),
        &reference,
        Harness::Continue,
        &world.base.join("output"),
        &source,
    )
}

#[test]
fn same_target_complete_lm_variants_preserve_owned_ir_wire_and_source_errors() -> Result<()> {
    let world = crate::test_world("same-target-lm");
    let body = "lmstudio λ\n".repeat(12000);
    let source = world.base.join("session.conversation.json");
    fs::write(
        &source,
        serde_json::to_vec(
            &json!({"name":"explicit","createdAt":1791892800000_i64,"pinned":true,"systemPrompt":"system","messages":[
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":body}]}]},
                {"currentlySelected":1,"versions":[
                    {"type":"multiStep","role":"assistant","steps":[{"type":"contentBlock","content":[{"type":"text","text":body}],"stepIdentifier":"1791892800001-0.x","genInfo":{"indexedModelIdentifier":"alternate-model","stats":{"promptTokensCount":7,"predictedTokensCount":2}}}]},
                    {"type":"multiStep","role":"assistant","steps":[{"type":"contentBlock","content":[{"type":"text","text":body}],"style":{"type":"thinking"},"genInfo":{"indexedModelIdentifier":"selected-model"}},{"type":"contentBlock","content":[{"type":"text","text":"answer"}]}]},
                    {"type":"singleStep","role":"assistant","content":[{"type":"text","text":"third alternative"}]}
                ]}
            ]}),
        )?,
    )?;
    let reference = SessionRef::new("synthetic-source", Harness::LmStudio, &source);
    check(
        &crate::harness::lmstudio::LmStudio::detect(),
        &reference,
        Harness::LmStudio,
        &world.base.join("output"),
        &source,
    )
}
