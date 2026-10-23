//! Bounded semantic body and label projections; no embedding model is needed to test them.
use anyhow::{Context, Result};
#[cfg(test)]
use cv_core::Text;
use cv_core::{Block, Flow, Message, MessageSink, Resolver, Role};

/// Read only the semantic projection demanded for one discovered session.
/// `head` bytes of the body are read: what the model embeds ([`crate::semantic`]'s `input_chars`).
pub(crate) fn read_corpus_doc(r: &cv_core::SessionRef, head: usize) -> Result<(Doc, cv_core::source::ReadSet)> {
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    let mut sink = DocSink::for_ref(r, &*adapter, head);
    let meta = cv_core::stream::stream_counted(
        &*adapter,
        r,
        &cv_core::Demand::LEAN.with_spans(),
        &mut sink,
        cv_core::stream::Pass::RECORD,
    )
    .with_context(|| format!("reading semantic corpus session {} ({})", r.id, r.harness))?
    .session;
    let title = cv_core::label_from(meta.title.as_deref(), sink.first_prompt.as_deref());
    Ok((
        Doc {
            title: Some(title),
            body: sink.body.into_string(),
        },
        sink.reads,
    ))
}

/// What the **semantic** path ([`semantic::embed_all`]) distills from one session beside the
/// discovered [`cv_core::SessionRef`] it embeds: the label hits show and the body it embeds.
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub title: Option<String>,
    pub body: String,
}

use cv_core::projection::Head;

/// What a session's vector is made of: the head of its own exchange, as far as the model reads it
/// ([`cv_core::MessageKind::is_exchange`], minus turns a fork copied from its source), as the
/// full-text `body` field holds it, and its first [`cv_core::Message::prompt_title`].
pub(super) struct DocSink {
    pub(super) body: Head,
    pub(super) first_prompt: Option<String>,
    /// Turns stamped before this were copied from another session ([`cv_core::Adapter::copied_before`]).
    pub(super) copied_before: Option<chrono::DateTime<chrono::Utc>>,
    pub(super) reads: cv_core::source::ReadSet,
    resolver: Resolver,
    error: Option<anyhow::Error>,
    failed: bool,
}
impl DocSink {
    fn for_ref(r: &cv_core::SessionRef, adapter: &dyn cv_core::Adapter, head: usize) -> Self {
        let mut sink = Self::new(Resolver::new(Some(r.path.clone())), head);
        sink.copied_before = adapter.copied_before(r, &mut sink);
        sink
    }

    pub(super) fn new(resolver: Resolver, head: usize) -> Self {
        Self {
            body: Head::new(head),
            first_prompt: None,
            copied_before: None,
            reads: Default::default(),
            resolver,
            error: None,
            failed: false,
        }
    }
    fn read_text(&mut self, text: &cv_core::Text) {
        match text {
            cv_core::Text::Span(span) => {
                if let Some(path) = &span.source {
                    self.reads.record(path);
                }
            }
            cv_core::Text::Source(source) => {
                self.reads.record(source.path());
                if let Some(reads) = source.dependencies() {
                    self.reads.merge(reads);
                }
            }
            cv_core::Text::Inline(_) => {}
        }
    }
}

impl cv_core::projection::Searchable for DocSink {
    fn full(&self) -> bool {
        self.body.full()
    }
    fn str(&mut self, text: &str) {
        self.body.push(text);
    }
    fn text(&mut self, text: &cv_core::Text, resolver: &Resolver, _: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.read_text(text);
        self.body.try_push_text(text, resolver)
    }
    fn json(&mut self, input: &cv_core::Json, _: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if !self.body.full() {
            if let cv_core::Json::Source(range) = input {
                self.reads.record(range.path());
                if let Some(reads) = range.dependencies() {
                    self.reads.merge(reads);
                }
            }
            self.body.push_json(input)?;
        }
        Ok(())
    }
    fn after_block(&mut self, block: &Block) -> Result<()> {
        if let Some(path) = block.persisted_output_path() {
            self.reads.record(std::path::Path::new(path));
            let head = cv_core::lazy::read_head_checked(std::path::Path::new(path), self.body.remaining())?;
            if let Some(text) = head.indexed_text(path) {
                self.body.push(&text);
                self.body.push("\n");
            }
        }
        Ok(())
    }
}

impl MessageSink for DocSink {
    fn dependency(&mut self, path: &std::path::Path) {
        self.reads.record(path);
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.failed |= self.error.is_some();
        self.error.take().or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("semantic projection previously failed"))
        })
    }

    fn message(&mut self, message: Message) -> Flow {
        if self.failed || self.error.is_some() {
            return Flow::Stop;
        }
        // Role is only a coarse gate; prompt_title below rejects non-prompt message kinds.
        if self.first_prompt.is_none() && message.role == Role::User {
            for block in &message.content {
                if let Block::Text { text } = block {
                    self.read_text(text);
                }
            }
            match message.prompt_title(&self.resolver) {
                Ok(title) => self.first_prompt = title,
                Err(error) => {
                    self.error = Some(error);
                    return Flow::Stop;
                }
            }
        }
        let copied = matches!((message.timestamp, self.copied_before), (Some(at), Some(before)) if at < before);
        if message.kind.is_exchange() && !copied {
            let resolver = std::mem::replace(&mut self.resolver, Resolver::new(None));
            self.error = cv_core::projection::searchable(&message.content, &resolver, self, &mut || Ok(())).err();
            self.resolver = resolver;
        }
        if self.error.is_some() || (self.body.full() && self.first_prompt.is_some()) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A head size for these tests; production derives it from the model (`input_chars`).
    const TEST_HEAD: usize = 64 * 1024;

    fn text(value: impl Into<String>) -> Block {
        Block::Text {
            text: Text::Inline(value.into()),
        }
    }
    fn tool(value: serde_json::Value) -> Block {
        Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            input: value.into(),
            namespace: None,
        }
    }
    fn message(role: Role, content: Vec<Block>) -> Message {
        let mut message = Message::new(role);
        message.content = content;
        message
    }
    fn sink() -> DocSink {
        DocSink::new(Resolver::new(None), TEST_HEAD)
    }
    fn prefix(text: &str, limit: usize) -> &str {
        let mut end = limit.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    #[test]
    fn semantic_source_text_bounds_heads_and_retains_full_conditional_guard() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-carrier-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("text.json");
        let database = dir.join("database");
        let wal = dir.join("database-wal");
        let conditional = dir.join("optional-sidecar");
        let decoded = format!("source label\n{}", "é🙂\\\"\n".repeat(100_000));
        std::fs::write(&path, serde_json::to_vec(&decoded).unwrap()).unwrap();
        std::fs::write(&database, "database identity").unwrap();
        for dependency in [&database, &wal, &conditional] {
            let mut guard = cv_core::source::ReadSet::default();
            for dependency in [&database, &wal, &conditional] {
                guard.record(dependency);
            }
            let value =
                cv_core::json::Json::source_with_reads(&path, 0, std::fs::metadata(&path).unwrap().len(), &guard)
                    .unwrap();
            let text = value.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
            assert!(matches!(text, Text::Source(_)));
            let mut projected = sink();
            assert_eq!(
                projected.message(message(Role::User, vec![Block::Text { text: text.clone() }])),
                Flow::Stop
            );
            assert!(projected.take_error().is_none());
            assert_eq!(projected.body.as_str(), prefix(&decoded, TEST_HEAD));
            assert!(projected.body.capacity() <= TEST_HEAD);
            assert_eq!(projected.first_prompt, Some(cv_core::ir::truncate(&decoded, 80)));
            assert!(projected.reads.fresh(&mut cv_core::source::Probe::default()));
            std::fs::write(dependency, "dependency changed or appeared").unwrap();
            assert!(!projected.reads.fresh(&mut cv_core::source::Probe::default()));
            let mut stale = sink();
            stale.body = Head::new(7);
            assert_eq!(
                stale.message(message(Role::Assistant, vec![Block::Text { text }])),
                Flow::Stop
            );
            assert!(stale.take_error().is_some());
            assert_eq!(stale.message(message(Role::User, vec![self::text("late")])), Flow::Stop);
            assert!(stale.take_error().is_some());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_source_json_retains_auxiliary_guard_after_capped_projection() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-json-guard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("input.json");
        let conditional = dir.join("sidecar");
        let raw = serde_json::to_vec(&json!({"arg":"x".repeat(200_000)})).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let mut guard = cv_core::source::ReadSet::default();
        guard.record(&conditional);
        let input = cv_core::json::Json::source_with_reads(&path, 0, raw.len() as u64, &guard).unwrap();
        let mut m = message(Role::Assistant, vec![tool(json!(null))]);
        if let Block::ToolUse { input: value, .. } = &mut m.content[0] {
            *value = input;
        }
        let mut projected = sink();
        projected.body = Head::new(32);
        projected.message(m.clone());
        assert!(projected.take_error().is_none());
        assert_eq!(projected.body.len(), 32);
        std::fs::write(&conditional, "now adopted").unwrap();
        assert!(!projected.reads.fresh(&mut cv_core::source::Probe::default()));
        let mut stale = sink();
        stale.body = Head::new(32);
        stale.message(m);
        assert!(stale.take_error().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_demanded_text_missing_or_truncated_failure_stays_latched() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-text-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("span");
        for role in [Role::User, Role::Assistant] {
            for truncated in [false, true] {
                if truncated {
                    std::fs::write(&path, "short").unwrap();
                }
                let mut sink = sink();
                let text = Text::Span(cv_core::Span {
                    source: Some(path.clone()),
                    offset: 0,
                    len: 8192,
                    escaped: false,
                });
                assert!(matches!(
                    sink.message(message(role, vec![Block::Text { text }])),
                    Flow::Stop
                ));
                assert!(sink.take_error().is_some());
                let saved = sink.body.as_str().to_owned();
                assert!(matches!(
                    sink.message(message(Role::User, vec![super::tests::text("later")])),
                    Flow::Stop
                ));
                assert_eq!(sink.body.as_str(), saved);
                assert!(sink.take_error().is_some());
                if truncated {
                    std::fs::remove_file(&path).unwrap();
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_optional_output_absent_or_unreadable_leaves_the_session_readable() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-output-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let block = |path: &std::path::Path| Block::ToolResult {
            tool_use_id: "call".into(),
            content: "stub".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: Some(json!({"persistedOutput": {"path": path}})),
        };
        let mut absent = sink();
        absent.message(message(Role::Assistant, vec![block(&dir.join("missing"))]));
        assert!(absent.take_error().is_none());
        assert_eq!(absent.body.as_str(), "stub\n");
        // Not a regular file: the output is noted as unreadable and the session read on.
        let mut unreadable = sink();
        assert!(matches!(
            unreadable.message(message(Role::Assistant, vec![block(&dir)])),
            Flow::Continue
        ));
        assert!(unreadable.take_error().is_none());
        assert!(
            unreadable.body.as_str().contains("unreadable"),
            "{}",
            unreadable.body.as_str()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_projection_tracks_selected_json_text_and_absent_outputs() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-reads-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let json_path = dir.join("input.json");
        let text_path = dir.join("text");
        let sidecar = dir.join("missing");
        std::fs::write(&json_path, "{\"arg\":true}").unwrap();
        std::fs::write(&text_path, "\"label\"").unwrap();
        let mut m = message(
            Role::User,
            vec![
                Block::Text {
                    text: Text::Span(cv_core::Span {
                        source: Some(text_path.clone()),
                        offset: 1,
                        len: 5,
                        escaped: false,
                    }),
                },
                tool(json!(null)),
                Block::ToolResult {
                    tool_use_id: "call".into(),
                    content: "stub".into(),
                    is_error: false,
                    tool_name: None,
                    status: None,
                    details: Some(json!({"persistedOutput": {"path": sidecar}})),
                },
            ],
        );
        if let Block::ToolUse { input, .. } = &mut m.content[1] {
            *input = cv_core::json::Json::source(&json_path, 0, 12).unwrap();
        }
        let mut sink = sink();
        sink.message(m);
        assert!(sink.take_error().is_none());
        assert!(sink.reads.fresh(&mut cv_core::source::Probe::default()));
        std::fs::write(&sidecar, "adopted output").unwrap();
        assert!(!sink.reads.fresh(&mut cv_core::source::Probe::default()));
        std::fs::remove_file(&sidecar).unwrap();
        // Each dependency is captured before its read, including the external label span.
        std::fs::write(&text_path, "new label").unwrap();
        assert!(!sink.reads.fresh(&mut cv_core::source::Probe::default()));
        std::fs::write(&json_path, "null").unwrap();
        assert!(!sink.reads.fresh(&mut cv_core::source::Probe::default()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_head_has_canonical_blocks_and_prompt_label() {
        let messages = [
            message(
                Role::Assistant,
                vec![
                    tool(json!({"a": [1, true, "é\n\""], "z": null})),
                    Block::Thinking {
                        text: "thought".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::File {
                        path: Some("a.rs".into()),
                        source: None,
                        mime: None,
                    },
                ],
            ),
            message(Role::User, vec![text(""), text("first\nuser"), text("second")]),
        ];
        let mut actual = sink();
        for m in messages {
            actual.message(m);
        }
        assert_eq!(
            actual.body.as_str(),
            "tool {\"a\":[1,true,\"é\\n\\\"\"],\"z\":null}\nthought\na.rs\n\nfirst\nuser\nsecond\n"
        );
        assert_eq!(actual.first_prompt.as_deref(), Some("first user second"));
    }

    #[test]
    fn semantic_head_source_json_preserves_prefix_and_reports_changed_source() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-json-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("input.json");
        let raw = "{\"arg\":\"\\u00e9\\n\\\"\",\"tail\":true}";
        std::fs::write(&path, raw).unwrap();
        let json = cv_core::json::Json::source(&path, 0, raw.len() as u64).unwrap();
        let mut message = message(Role::Assistant, vec![tool(json!(null))]);
        if let Block::ToolUse { input, .. } = &mut message.content[0] {
            *input = json;
        }
        let mut expected = String::new();
        message.append_searchable(&mut expected).unwrap();
        for limit in 0..expected.len() {
            let mut actual = sink();
            actual.body = Head::new(limit);
            actual.message(message.clone());
            assert!(actual.take_error().is_none());
            assert_eq!(actual.body.as_str(), prefix(&expected, limit), "limit {limit}");
        }
        std::fs::write(&path, "null").unwrap();
        let mut actual = sink();
        actual.message(message);
        assert!(actual.take_error().unwrap().to_string().contains("JSON source changed"));
        let saved = actual.body.as_str().to_owned();
        assert!(matches!(
            actual.message(super::tests::message(Role::User, vec![text("later")])),
            Flow::Stop
        ));
        assert_eq!(actual.body.as_str(), saved);
        assert!(actual.take_error().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_head_giant_inline_arguments_names_and_paths_stay_bounded() {
        for size in [16 * 1024 * 1024, 64 * 1024 * 1024] {
            for content in [
                text("x".repeat(size)),
                tool(json!({"arg": "é\\\n".repeat(size / 4)})),
                Block::File {
                    path: Some("p".repeat(size)),
                    source: None,
                    mime: None,
                },
                Block::ToolUse {
                    id: "call".into(),
                    name: "n".repeat(size),
                    input: json!(null).into(),
                    namespace: None,
                },
            ] {
                let mut actual = sink();
                actual.message(message(Role::User, vec![content]));
                assert!(actual.body.len() <= TEST_HEAD);
                assert_eq!(actual.body.capacity(), TEST_HEAD);
                assert!(actual.first_prompt.is_none_or(|text| text.chars().count() <= 80));
            }
        }
    }

    #[test]
    fn semantic_head_caps_json_utf8_and_punctuation_as_a_single_prefix() {
        let m = message(
            Role::Assistant,
            vec![
                text("αβ"),
                tool(json!({"arg": "é\"\n".repeat(100)})),
                text("must not skip ahead"),
            ],
        );
        let mut expected = String::new();
        m.append_searchable(&mut expected).unwrap();
        for limit in 0..expected.len() {
            let mut actual = sink();
            actual.body = Head::new(limit);
            actual.message(m.clone());
            assert_eq!(actual.body.as_str(), prefix(&expected, limit), "limit {limit}");
        }
    }

    #[test]
    fn semantic_head_embeds_the_exchange_and_titles_the_first_prompt() {
        let mut instructions = message(Role::User, vec![text("# AGENTS.md instructions for /work")]);
        instructions.kind = cv_core::MessageKind::InjectedContext;
        let mut actual = sink();
        actual.message(instructions);
        actual.message(message(Role::User, vec![text(" ".repeat(10000))]));
        assert!(actual.first_prompt.is_none());
        actual.message(message(Role::User, vec![text("chosen")]));
        assert_eq!(actual.first_prompt.as_deref(), Some("chosen"));
        assert!(!actual.body.as_str().contains("AGENTS.md"));
        assert!(actual.body.as_str().contains("chosen"));
    }

    #[test]
    fn semantic_transcript_head_is_independent_of_json_escaping() {
        let world = cv_testkit::World::new("semantic-decoded-head");
        for scalar in ["x", "\\u0078"] {
            let path = world.proj.join("session.jsonl");
            let raw = scalar.repeat(100_000);
            std::fs::write(
                &path,
                format!(
                    "{{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"head\",\"message\":{{\"role\":\"user\",\"content\":\"{raw}\"}}}}\n"
                ),
            )
            .unwrap();
            let r = cv_core::SessionRef::new("head", cv_core::Harness::Claude, &path);
            let (doc, reads) = read_corpus_doc(&r, TEST_HEAD).unwrap();
            assert_eq!(doc.body.len(), 64 * 1024);
            assert!(doc.body.bytes().all(|byte| byte == b'x'));
            assert!(reads.fresh(&mut cv_core::source::Probe::default()));
        }
    }

    /// The committed Claude corpus copies one CLAUDE.md into 18 transcripts; a vector embeds it
    /// only where the exchange says it (`dfa91e7c`'s reply quotes it), and the fork's copied
    /// turns (`b3a058ca`) are its source's alone, as the full-text `body` field holds them.
    #[test]
    fn corpus_semantic_bodies_embed_the_exchange_not_copied_instructions() {
        let home = cv_testkit::temp_dir("semantic-corpus-home");
        let scope = (*cv_core::env::Env::current()).clone().enter();
        scope.set("CLUSTERVISION_HOME", &home);
        let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../cv-core/tests/corpus/claude/2.1.296/.claude/projects/-home-corpus-project");
        let mut quoted = Vec::new();
        for path in std::fs::read_dir(&project).unwrap().map(|e| e.unwrap().path()) {
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let id = path.file_stem().unwrap().to_str().unwrap().to_owned();
            let r = cv_core::SessionRef::new(&id, cv_core::Harness::Claude, &path);
            let (doc, _) = crate::read_corpus_doc(&r, TEST_HEAD).unwrap();
            if doc.body.contains("corpus test repository") {
                quoted.push(id[..8].to_owned());
            }
            if id.starts_with("b3a058ca") {
                assert!(!doc.body.contains("Which language did you use first"), "a copied turn");
            }
        }
        assert_eq!(quoted, ["dfa91e7c"]);
        drop(scope);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn semantic_head_sidecar_and_stop_share_the_body_budget() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("output");
        let output = "é".repeat(100000);
        std::fs::write(&path, &output).unwrap();
        let block = Block::ToolResult {
            tool_use_id: "call".into(),
            content: "stub".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: Some(json!({"persistedOutput": {"path": path}})),
        };
        let mut actual = sink();
        assert!(matches!(
            actual.message(message(Role::Assistant, vec![block])),
            Flow::Continue
        ));
        let projected = format!("stub\n{output}\n");
        assert_eq!(actual.body.as_str(), prefix(&projected, TEST_HEAD));
        assert!(actual.body.full());
        let saved = actual.body.as_str().to_owned();
        assert!(matches!(
            actual.message(message(Role::User, vec![text("label")])),
            Flow::Stop
        ));
        assert_eq!(actual.body.as_str(), saved);
        assert_eq!(actual.first_prompt.as_deref(), Some("label"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_head_lazy_spans_keep_a_bounded_prefix() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-head-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        // A span covers a JSON string's body.
        for (escaped, bytes) in [(true, b"\\u0061".repeat(20000)), (false, vec![b'a'; 100000])] {
            let path = dir.join("source");
            std::fs::write(&path, [&b"\""[..], &bytes, b"\""].concat()).unwrap();
            let resolver = Resolver::new(Some(path));
            let span = cv_core::Span {
                source: None,
                offset: 1,
                len: bytes.len() as u64,
                escaped,
            };
            let expected = resolver.try_resolve_prefix(&span, TEST_HEAD as u64).unwrap();
            let mut actual = DocSink::new(Resolver::new(Some(dir.join("source"))), TEST_HEAD);
            let prompt = message(Role::User, vec![Block::Text { text: Text::Span(span) }]);
            let expected_label = prompt.prompt_title(&resolver).unwrap();
            actual.message(prompt);
            let mut projected = prefix(&expected, TEST_HEAD).to_owned();
            if projected.len() < TEST_HEAD && expected.len() <= TEST_HEAD {
                projected.push('\n');
            }
            assert_eq!(actual.body.as_str(), projected);
            assert!(actual.body.len() <= TEST_HEAD);
            assert_eq!(actual.first_prompt, expected_label);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
