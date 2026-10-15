//! Legacy single-document rollouts (2025: `{session, items[]}` or a bare `items` array) read
//! through the same record codec as modern rollouts: the session object is a `session_meta`
//! payload, each item a `response_item` payload. Only one item's demanded fields are owned at once.
use super::*;
use crate::harness::store::{document::Fields, jsonl::ranges};
use crate::json::InvalidJson;

/// Value acceptance is whole-document, including ignored descendants. Invalid JSON remains an
/// empty session; read errors (including invalid UTF-8) remain fatal, as with read_to_string.
fn read<T>(path: &Path, f: impl for<'a> FnOnce(&JsonSource, JsonView<'a>) -> Result<T>) -> Result<Option<T>> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    source.check_metadata(file.metadata()?)?;
    if file.metadata()?.len() == 0 {
        source.check()?;
        return Ok(None);
    }
    let mut entered = false;
    match source.read_document(|view| {
        entered = true;
        f(&source, view)
    }) {
        Ok(value) => Ok(Some(value)),
        Err(error) if !entered && error.is::<InvalidJson>() => {
            let valid = ranges::valid_utf8(&mut BufReader::new(file))?;
            source.check()?;
            anyhow::ensure!(valid, "reading {}: stream did not contain valid UTF-8", path.display());
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn items_of(root: JsonView<'_>) -> Result<Option<crate::json::JsonArray<'_>>> {
    if let Some(items) = root.get("items")? {
        if let Some(array) = items.array()? {
            return Ok(Some(array));
        }
    }
    root.array()
}

/// A whole document into `reader`: its session object, then every item. Legacy rollouts have no
/// modern event authority, inheritance or complete carriers.
pub(super) fn items(reader: &mut Reader<'_>, root: JsonView<'_>) -> Result<()> {
    reader.meta(root.get("session")?)?;
    if let Some(items) = items_of(root)? {
        for item in items {
            reader.item(Some(item?), None)?;
        }
    }
    Ok(())
}

/// Discovery deliberately keeps the legacy policy: only session timestamps, no cwd, no event
/// authority, and turns counted by the record codec's heads demand, one item at a time.
pub(super) fn scan(path: &Path) -> Result<CodexScan> {
    Ok(read(path, |_source, root| {
        let mut scan = CodexScan::default();
        let meta = root
            .get("session")?
            .map(|v| Fields::parse(v, &["id", "timestamp", "agent_path"]))
            .transpose()?
            .flatten();
        let mut ctx = CodexCtx::default();
        if let Some(meta) = meta {
            scan.id = meta.string("id")?.unwrap_or_default();
            scan.created_at = meta.string("timestamp")?.as_deref().and_then(parse_ts);
            scan.updated_at = scan.created_at;
            ctx.agent_path = meta.string("agent_path")?;
        }
        let mut session = Session::new(String::new(), Harness::Codex);
        let mut out = Vec::new();
        if let Some(items) = items_of(root)? {
            for item in items {
                let item = item?;
                Reader {
                    session: &mut session,
                    ctx: &mut ctx,
                    out: &mut out,
                    has_events: false,
                    demand: Demand {
                        spans: false,
                        heads: true,
                    },
                    source: None,
                }
                .item(Some(item), None)?;
                scan.message_count += out.drain(..).filter(|m| m.is_turn()).count();
                if scan.title.is_none() {
                    if let Some(fields) = Fields::parse(item, &["type", "role", "content"])? {
                        let user = fields.string("role")?.as_deref().unwrap_or("user") == "user";
                        if fields.string("type")?.as_deref() == Some("message") && user {
                            scan.consider_title(title_of(fields.get("content"), None)?);
                        }
                    }
                }
            }
        }
        Ok(scan)
    })?
    .unwrap_or_default())
}

pub(super) fn stream(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
    let empty = || Session {
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Codex)
    };
    let parsed = read(&r.path, |source, root| {
        let mut s = Session {
            source_path: Some(r.path.clone()),
            ..Session::new(String::new(), Harness::Codex)
        };
        let mut ctx = CodexCtx::default();
        let mut scratch = Vec::new();
        fn reader<'s>(
            session: &'s mut Session,
            ctx: &'s mut CodexCtx,
            out: &'s mut Vec<Message>,
            opts: &ParseOptions,
            source: &'s JsonSource,
        ) -> Reader<'s> {
            Reader {
                session,
                ctx,
                out,
                has_events: false,
                demand: Demand::of(opts),
                source: Some(source),
            }
        }
        reader(&mut s, &mut ctx, &mut scratch, opts, source).meta(root.get("session")?)?;
        if s.id.is_empty() {
            s.id = r.id.clone();
        }
        sink.meta(&s);
        if let Some(error) = sink.take_error() {
            return Err(error);
        }
        if let Some(items) = items_of(root)? {
            for item in items {
                reader(&mut s, &mut ctx, &mut scratch, opts, source).item(Some(item?), None)?;
                for message in scratch.drain(..) {
                    if sink.try_message(message)? == Flow::Stop {
                        return Ok(s);
                    }
                }
            }
        }
        Ok(s)
    })?;
    match parsed {
        Some(s) => Ok(s),
        None => crate::stream::replay(empty(), sink),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collected(path: &Path, opts: &ParseOptions) -> Session {
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let mut sink = crate::stream::CollectSink::default();
        let mut session = Codex { roots: Vec::new() }.stream(&r, opts, &mut sink).unwrap();
        session.messages = sink.messages;
        session
    }

    #[test]
    fn checked_legacy_acceptance_stop_and_source_failures_are_preserved() {
        let world = crate::test_world("codex-legacy-acceptance");
        let path = world.base.join("rollout-legacy.json");
        let valid_prefix = r#"{"items":[{"type":"message","content":"visible"}],"ignored":"#;
        let deep = format!("{valid_prefix}{}0{} }}", "[".repeat(130), "]".repeat(130));
        for raw in [
            "",
            " ",
            "false",
            "{}",
            r#"{"items":[{"type":"message","content":"visible"}],"ignored":"\ud800"}"#,
            &deep,
            r#"{"items":[{"type":"message","content":"visible"}],"ignored":1e9999}"#,
            r#"{"items":[{"type":"message","content":"visible"}]"#,
        ] {
            // Not exactly a serde Value (or no items): an empty session, an empty listing.
            fs::write(&path, raw).unwrap();
            let actual = collected(&path, &ParseOptions::lazy());
            assert!(actual.messages.is_empty(), "{raw}");
            assert_eq!(actual.id, "fallback");
            assert_eq!(scan(&path).unwrap(), CodexScan::default(), "{raw}");
        }
        let mut bad_utf8 = valid_prefix.as_bytes().to_vec();
        bad_utf8.extend_from_slice(b"\"\xff\"}");
        fs::write(&path, bad_utf8).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, &path);
        let mut sink = crate::stream::CollectSink::default();
        assert!(stream(&r, &ParseOptions::lazy(), &mut sink).is_err());
        assert!(sink.messages.is_empty());
        assert!(scan(&path).is_err());

        let raw = json!({"session":{"id":"legacy","timestamp":"2025-05-05T19:24:54Z"},"items":[
            {"type":"message","content":"body".repeat(8192)}, {"type":"message","content":"second"},
        ]})
        .to_string();
        fs::write(&path, &raw).unwrap();
        let mut saved = None;
        let mut stop = |m: Message| {
            saved = Some(m);
            Flow::Stop
        };
        let meta = stream(&r, &ParseOptions::lazy(), &mut stop).unwrap();
        assert_eq!(meta.id, "legacy");
        let saved = saved.unwrap();
        assert!(matches!(saved.content.first(), Some(Block::Text { text }) if text.inline_str().is_none()));
        fs::write(&path, "replacement source").unwrap();
        if let Block::Text { text } = &saved.content[0] {
            assert!(text.try_resolve(&crate::Resolver::new(Some(path.clone()))).is_err());
        }
        fs::write(&path, &raw).unwrap();
        let mut mutate = |_m: Message| {
            fs::write(&path, "replacement source").unwrap();
            Flow::Stop
        };
        assert!(stream(&r, &ParseOptions::lazy(), &mut mutate).is_err());
    }
}
