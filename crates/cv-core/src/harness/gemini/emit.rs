//! The Gemini emitter: an IR [`Session`] written into Gemini's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::EmitResult;
use crate::ir::{Block, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Emit a gemini-cli legacy chat recording: `out_dir/<sessionId>.json`, a single whole-file
/// `ConversationRecord` `{sessionId, projectHash, startTime, lastUpdated, messages[]}`. The reader
/// (`harness/gemini.rs`) requires the file to live under a `chats/` dir to be picked up by
/// `discover`, but `parse_all_str` / `parse` will read it directly from any path.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    stream::emit(session, out_dir, opts)
}

mod stream;

/// Bind `cwd` to a slug dir under gemini's `<globalTemp>` (=`out_dir`), replicating
/// `ProjectRegistry.getShortId`/`claimNewSlug`. Gemini owns a slug dir via a `.project_root` marker
/// file containing the project path; on launch it RE-derives the slug and, if our dir lacks a marker
/// (or the projects.json entry can't be verified), reassigns a fresh slug — so writing only
/// projects.json is not enough. We therefore: pick the cwd basename (suffixing `-2`, `-3`… if a
/// DIFFERENT path already owns that dir's marker), write the `.project_root` marker, and record the
/// projects.json mapping (the fast path; gemini heals from the marker regardless). After this,
/// `gemini --resume`/`--list-sessions` discover the session. Returns the slug, `None` if `cwd` empty.
fn gemini_register_project(global_tmp: &Path, cwd: &str) -> Result<Option<String>> {
    if cwd.is_empty() {
        return Ok(None);
    }
    const MARKER: &str = ".project_root";
    // Reuse the slug whose marker already owns this cwd, if any (gemini's findExistingSlugForPath).
    let owned_by = |slug_dir: &Path| -> Option<String> {
        fs::read_to_string(slug_dir.join(MARKER))
            .ok()
            .map(|s| s.trim().to_string())
    };
    if let Ok(entries) = fs::read_dir(global_tmp) {
        for e in entries.flatten() {
            if owned_by(&e.path()).as_deref() == Some(cwd) {
                return Ok(e.file_name().to_str().map(str::to_string));
            }
        }
    }
    // Otherwise claim the basename, skipping dirs whose marker owns a different path.
    let base = Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());
    let mut slug = base.clone();
    let mut n = 2;
    loop {
        let dir = global_tmp.join(&slug);
        match owned_by(&dir) {
            Some(owner) if owner != cwd => {
                slug = format!("{base}-{n}");
                n += 1;
            }
            _ => break, // free, or already ours
        }
    }
    let slug_dir = global_tmp.join(&slug);
    fs::create_dir_all(&slug_dir).with_context(|| format!("creating Gemini project {}", slug_dir.display()))?;
    let marker = slug_dir.join(MARKER);
    crate::publish::publish_reader(&marker, "stage", cwd.as_bytes(), crate::publish::Claim::Replace)
        .with_context(|| format!("writing Gemini project marker {}", marker.display()))?;
    // Record the projects.json fast-path mapping too (best-effort).
    if let Some(parent) = global_tmp.parent() {
        let projects_json = parent.join("projects.json");
        let mut doc: Value = match fs::read_to_string(&projects_json) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(doc) if doc.is_object() => doc,
                // The required marker is enough for native recovery. Never replace an unreadable
                // or unrecognized existing registry with an invented empty mapping.
                _ => return Ok(Some(slug)),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(_) => return Ok(Some(slug)),
        };
        let projects = doc
            .as_object_mut()
            .unwrap()
            .entry("projects")
            .or_insert_with(|| json!({}));
        if let Some(m) = projects.as_object_mut() {
            m.insert(cwd.to_string(), json!(slug));
        }
        if let Ok(s) = serde_json::to_string_pretty(&doc) {
            let _ =
                crate::publish::publish_reader(&projects_json, "stage", s.as_bytes(), crate::publish::Claim::Replace);
        }
    }
    Ok(Some(slug))
}

#[cfg(test)]
mod checked_tests {
    use super::*;
    use crate::{
        ir::{Message, Usage},
        Harness, Json, SessionRef,
    };

    #[test]
    fn project_registration_preserves_unrecognized_existing_registry() {
        let root = std::env::temp_dir().join(format!("cv-gemini-registry-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let registry = root.join("projects.json");
        for original in ["{broken registry", "[\"unexpected schema\"]"] {
            fs::write(&registry, original).unwrap();
            assert_eq!(
                gemini_register_project(&root.join("tmp"), "/synthetic/project").unwrap(),
                Some("project".into())
            );
            assert_eq!(fs::read_to_string(&registry).unwrap(), original);
            assert_eq!(
                fs::read_to_string(root.join("tmp/project/.project_root")).unwrap(),
                "/synthetic/project"
            );
            fs::remove_dir_all(root.join("tmp")).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_project_marker_failure_prevents_transcript_publication() {
        let dir = std::env::temp_dir().join(format!("cv-gemini-marker-{}", Uuid::new_v4()));
        fs::create_dir_all(dir.join("project/.project_root")).unwrap();
        let mut session = Session::new("source", Harness::Gemini);
        session.cwd = Some("/synthetic/project".into());
        let result = emit(
            &session,
            &dir,
            &EmitOptions {
                new_id: Some("destination".into()),
                ..Default::default()
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("project marker"));
        assert!(!dir.join("project/chats").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    fn normalized(path: &Path) -> Result<Vec<u8>> {
        let mut value: Value = serde_json::from_slice(&fs::read(path)?)?;
        for (i, message) in value["messages"].as_array_mut().unwrap().iter_mut().enumerate() {
            assert!(Uuid::parse_str(message["id"].as_str().unwrap()).is_ok());
            message["id"] = json!(format!("generated-{i}"));
        }
        Ok(serde_json::to_vec_pretty(&value)?)
    }
    fn result(id: &str, body: &str, error: bool) -> Block {
        Block::ToolResult {
            tool_use_id: id.into(),
            content: body.into(),
            is_error: error,
            tool_name: None,
            status: None,
            details: None,
        }
    }
    #[test]
    fn checked_gemini_matches_frozen_pretty_all_modes_and_fields() -> Result<()> {
        let world = crate::test_world("gemini-wire-oracle");
        let mut frozen = crate::emit::grok_qwen_tests::Frozen::new("gemini-fixture");
        let mut session = Session::new("source", Harness::Claude);
        let ts = "2026-10-12T12:00:00Z".parse()?;
        session.created_at = Some(ts);
        session.updated_at = Some(ts);
        session.cwd = Some("/synthetic/checked-gemini".into());
        session.title = Some("title λ".into());
        session.model = Some("session-model".into());
        let mut user = Message::new(Role::User);
        user.timestamp = Some(ts);
        user.content = vec![
            Block::Text {
                text: "prompt\n雪".into(),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: None,
            },
            Block::Image {
                media_type: None,
                data_ref: None,
            },
            Block::File {
                mime: Some("text/plain".into()),
                path: Some("fallback".into()),
                source: Some("file://λ".into()),
            },
        ];
        let mut assistant = Message::new(Role::Assistant);
        assistant.timestamp = Some(ts);
        assistant.model = Some("per-message-model".into());
        assistant.usage = Some(Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            cache_read_tokens: Some(2),
            ..Default::default()
        });
        assistant.content = vec![
            Block::Thinking {
                text: "  reasoning\nλ".into(),
                signature: Some("sig".into()),
                encrypted: None,
                redacted: false,
            },
            Block::Thinking {
                text: " ".into(),
                signature: Some("only".into()),
                encrypted: None,
                redacted: false,
            },
            Block::Thinking {
                text: "".into(),
                signature: None,
                encrypted: Some("opaque".into()),
                redacted: false,
            },
            Block::Thinking {
                text: "".into(),
                signature: None,
                encrypted: None,
                redacted: true,
            },
            Block::Text {
                text: "answer\"\\\t".into(),
            },
            Block::ToolUse {
                id: "paired".into(),
                name: "tool".into(),
                namespace: None,
                input: json!({"β":[1,true,null],"a":{"z":"\""}}).into(),
            },
        ];
        let mut tools = Message::new(Role::Tool);
        tools.timestamp = Some(ts);
        tools.content = vec![
            result("paired", "earlier", false),
            result("paired", "late error\nλ", true),
            result("orphan", "orphan output", false),
        ];
        let mut thinking_only = Message::new(Role::Assistant);
        thinking_only.content = vec![Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        }];
        session.messages = vec![user, assistant, tools, Message::new(Role::System), thinking_only];
        session
            .harness_extra_mut(Harness::Claude)
            .insert("native".into(), json!({"metadata":"retained source"}));
        for mode in ThinkingMode::ALL {
            let opts = EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                thinking: mode,
                ..Default::default()
            };
            let after = crate::emit::emit(
                &session,
                Harness::Gemini,
                &world.base.join(format!("checked-{mode}")),
                &opts,
            )?;
            let bytes = normalized(&after.path)?;
            frozen.insert(
                mode.to_string(),
                json!([bytes.len(), crate::digest::sha256_hex(&bytes)]),
            );
            // Original serialized order/whitespace is already serde's pretty normalization.
            let raw = fs::read(&after.path)?;
            let parsed: Value = serde_json::from_slice(&raw)?;
            assert_eq!(raw, serde_json::to_vec_pretty(&parsed)?);
            assert!(!raw.ends_with(b"\n"));
            assert_eq!(
                session.harness_extra(Harness::Claude).unwrap()["native"]["metadata"],
                "retained source"
            );
        }
        frozen.check()
    }
    #[test]
    fn checked_gemini_guarded_text_json_fail_before_transcript_publication() -> Result<()> {
        let world = crate::test_world("gemini-guarded-publication");
        for text_leaf in [true, false] {
            let source = world.base.join(format!("leaf-{text_leaf}.json"));
            let wire = if text_leaf {
                json!("guarded λ\n".repeat(10000))
            } else {
                json!({"argument":"guarded λ\n".repeat(10000)})
            };
            let raw = serde_json::to_vec(&wire)?;
            fs::write(&source, &raw)?;
            let json = Json::source(&source, 0, raw.len() as u64)?;
            let mut message = Message::new(Role::Assistant);
            message.content.push(if text_leaf {
                Block::Text {
                    text: json.with_view(|v| v.source_text())?.unwrap(),
                }
            } else {
                Block::ToolUse {
                    id: "id".into(),
                    name: "name".into(),
                    namespace: None,
                    input: json,
                }
            });
            let mut session = Session::new("source", Harness::Claude);
            session.messages.push(message);
            let out = world.base.join(format!("target-{text_leaf}"));
            fs::create_dir_all(out.join("chats"))?;
            let path = out.join("chats/session-fixed.json");
            fs::write(&path, "original transcript")?;
            fs::write(&source, "changed source")?;
            assert!(emit(
                &session,
                &out,
                &EmitOptions {
                    new_id: Some("fixed".into()),
                    ..Default::default()
                }
            )
            .is_err());
            assert_eq!(fs::read_to_string(&path)?, "original transcript");
            assert!(!out.parent().unwrap().join("projects.json").exists());
        }
        Ok(())
    }
    #[test]
    fn checked_gemini_cross_harness_preparation_preserves_full_metadata_and_owned_defaults() -> Result<()> {
        let world = crate::test_world("gemini-preparation");
        let source = world.base.join("source.jsonl");
        let wire = json!({"type":"assistant","uuid":"a","sessionId":"source","timestamp":"2026-10-12T12:00:00Z","cwd":"/synthetic/gemini",
            "message":{"role":"assistant","model":"source-model","content":[{"type":"text","text":"metadata body λ".repeat(2000)}],"usage":{"input_tokens":7,"output_tokens":3}},
            "gitBranch":"branch","custom":"unknown native metadata"});
        fs::write(&source, format!("{wire}\n"))?;
        let adapter = crate::harness::claude::Claude::detect();
        let reference = SessionRef::new("source", Harness::Claude, &source);
        let mut prepared = crate::emit::parse_for_emit(&adapter, &reference, Harness::Gemini)?;
        assert!(prepared.messages[0]
            .content
            .iter()
            .any(|block| matches!(block,Block::Text {text} if text.is_span())));
        prepared.materialize()?;
        let full = crate::stream::collect_with(&adapter, &reference, &crate::ParseOptions::full())?;
        assert_eq!(serde_json::to_value(&prepared)?, serde_json::to_value(&full)?);
        let complete = crate::stream::collect_with(&adapter, &reference, &crate::ParseOptions::complete())?;
        assert!(complete.messages[0]
            .content
            .iter()
            .all(|block| !matches!(block,Block::Text {text} if text.is_span())));
        // Same-harness Gemini uses the existing complete/owned compatibility path.
        Ok(())
    }
}
