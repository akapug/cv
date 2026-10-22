//! Message frames and canonical artifact prefix ranges. JSON is serialized by serde; framing
//! only uses ASCII RS, which cannot occur unescaped in valid JSON. Backing storage is shared.
use super::*;
use cv_core::bytes::{ByteSource, ByteStage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FirstUser {
    /// One-based message position, so a retained prefix includes it when ordinal <= count.
    pub ordinal: usize,
    pub head: Option<String>,
}

/// Which spooled messages are turns ([`cv_core::Message::is_turn`]), one bit each: the turns of
/// any stable prefix of them, counted after the pass decides where that prefix ends.
#[derive(Default)]
pub(super) struct Turns(Vec<u64>);
impl Turns {
    fn push(&mut self, ordinal: usize, turn: bool) {
        if ordinal.is_multiple_of(64) {
            self.0.push(0);
        }
        if turn {
            *self.0.last_mut().unwrap() |= 1 << (ordinal % 64);
        }
    }
    /// The turns among the first `n` spooled messages.
    pub fn before(&self, n: usize) -> usize {
        let (whole, rest) = (n / 64, n % 64);
        let full: u32 = self.0.iter().take(whole).map(|word| word.count_ones()).sum();
        let part = self
            .0
            .get(whole)
            .filter(|_| rest > 0)
            .map_or(0, |word| (word & ((1 << rest) - 1)).count_ones());
        (full + part) as usize
    }
}

pub(super) struct Spooled {
    pub bytes: ByteSource,
    pub messages: usize,
    pub turns: Turns,
    pub first_user: Option<FirstUser>,
}

pub(super) struct Spool {
    stage: BufWriter<ByteStage>,
    resolver: cv_core::Resolver,
    messages: usize,
    turns: Turns,
    error: Option<anyhow::Error>,
    first_user: Option<FirstUser>,
}
impl Spool {
    pub fn new(path: &Path) -> Self {
        Self {
            stage: BufWriter::with_capacity(64 * 1024, ByteStage::default()),
            resolver: cv_core::Resolver::new(Some(path.to_owned())),
            messages: 0,
            turns: Turns::default(),
            error: None,
            first_user: None,
        }
    }
    pub fn finish(self) -> Result<Spooled> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let stage = self.stage.into_inner().map_err(|error| error.into_error())?;
        Ok(Spooled {
            bytes: stage.finish()?,
            messages: self.messages,
            turns: self.turns,
            first_user: self.first_user,
        })
    }
}
impl cv_core::MessageSink for Spool {
    fn meta(&mut self, session: &Session) {
        self.resolver = session.resolver();
    }
    fn message(&mut self, message: cv_core::Message) -> cv_core::Flow {
        if self.error.is_some() {
            return cv_core::Flow::Stop;
        }
        let result = (|| -> Result<()> {
            if self.first_user.is_none() {
                // A durable archive must fail on unreadable title text; unlike the display
                // fallback Session::synth_title, it cannot skip a failed prompt and publish.
                if let Some(head) = message.prompt_title(&self.resolver)? {
                    self.first_user = Some(FirstUser {
                        ordinal: self.messages + 1,
                        head: Some(head),
                    });
                }
            }
            let stage = &mut self.stage;
            cv_core::projection::write_frame(stage, &message, &self.resolver, None)?;
            self.turns.push(self.messages, message.is_turn());
            self.messages += 1;
            Ok(())
        })();
        match result {
            Ok(()) => cv_core::Flow::Continue,
            Err(error) => {
                self.error = Some(error);
                cv_core::Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::session_json::write_session;
    use cv_core::{Block, Flow, Message, MessageSink, Role};

    fn stage(session: &Session) -> (ByteSource, Option<FirstUser>) {
        let r = SessionRef::new(&session.id, session.harness, PathBuf::from("unused"));
        let mut spool = Spool::new(&r.path);
        spool.meta(session);
        for message in &session.messages {
            assert_eq!(spool.message(message.clone()), Flow::Continue);
        }
        let Spooled {
            bytes,
            messages: count,
            turns,
            first_user: first,
        } = spool.finish().unwrap();
        assert_eq!(count, session.messages.len());
        for n in 0..=count {
            assert_eq!(
                turns.before(n),
                session.messages[..n].iter().filter(|m| m.is_turn()).count()
            );
        }
        (bytes, first)
    }
    fn message(text: &str) -> Message {
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text { text: text.into() });
        message
    }

    #[test]
    fn turns_before_counts_across_words() {
        let mut turns = Turns::default();
        let flags: Vec<bool> = (0..200).map(|i| i % 3 == 0 || i == 64).collect();
        for (i, &flag) in flags.iter().enumerate() {
            turns.push(i, flag);
        }
        for n in 0..=flags.len() {
            assert_eq!(turns.before(n), flags[..n].iter().filter(|&&f| f).count());
        }
    }

    #[test]
    fn source_text_frames_and_labels_match_owned_and_reject_changed_guards() {
        let world = crate::test_world("archive-source-text");
        let path = world.home.join("source.json");
        let sidecar = world.home.join("optional-meta.json");
        let body = "  λ moon\n\"text\" ".repeat(10_000);
        let encoded = serde_json::to_vec(&body).unwrap();
        fs::write(&path, &encoded).unwrap();
        let mut guard = cv_core::source::ReadSet::default();
        guard.record(&sidecar);
        let json = cv_core::Json::source_with_reads(&path, 0, encoded.len() as u64, &guard).unwrap();
        let text = json.with_view(|v| v.source_text()).unwrap().unwrap();
        assert!(matches!(text, cv_core::Text::Source(_)));
        let mut owned = Session::new("source-label", Harness::Codex);
        owned.messages.push(message(&body));
        let mut lazy = owned.clone();
        lazy.messages[0].content = vec![Block::Text { text }];
        let (expected, label) = stage(&owned);
        let (actual, source_label) = stage(&lazy);
        let mut left = Vec::new();
        let mut right = Vec::new();
        expected.copy_to(&mut left).unwrap();
        actual.copy_to(&mut right).unwrap();
        assert_eq!(left, right);
        assert_eq!(label.unwrap().head, source_label.unwrap().head);
        fs::write(&sidecar, "new metadata").unwrap();
        let mut spool = Spool::new(&path);
        assert_eq!(spool.message(lazy.messages.remove(0)), Flow::Stop);
        assert!(spool.take_error().is_some());
    }

    #[test]
    fn first_nonempty_prompt_supplies_archive_label() {
        let mut session = Session::new("label", Harness::Claude);
        session.messages = vec![message(""), message("first substantive prompt")];
        let (_, first) = stage(&session);
        let first = first.unwrap();
        assert_eq!(first.ordinal, 2);
        assert_eq!(cv_core::label_from(None, first.head.as_deref()), session.label());
    }

    #[test]
    fn explicit_archive_title_precedes_prompt_label() {
        let mut session = Session::new("label", Harness::Claude);
        session.title = Some("explicit title".into());
        session.messages = vec![message("a different prompt")];
        let (_, first) = stage(&session);
        assert_eq!(
            cv_core::label_from(session.title.as_deref(), first.unwrap().head.as_deref()),
            "explicit title"
        );
    }

    #[test]
    fn derived_pretty_output_and_relocated_prefix_are_exact() {
        let root = std::env::temp_dir().join(format!("cv-archive-frame-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let mut session = Session::new("framing", Harness::Claude);
        session.title = Some("短い\n\"title\"".into());
        session
            .extra
            .insert("messages".into(), serde_json::json!({"messages":[null,{},["x"]]}));
        session.messages = vec![
            message(""),
            message("Unicode é🦀\n\u{1e}\\\""),
            message(&"wide\n".repeat(20_000)),
        ];
        for stable in 0..=session.messages.len() {
            let (bytes, _) = stage(&session);
            let mut header = session.clone();
            header.messages.clear();
            let mut full = Vec::new();
            let prefix = write_session(&mut full, &header, &bytes, None, Some(stable), session.messages.len()).unwrap();
            assert_eq!(full, serde_json::to_vec_pretty(&session).unwrap());
            if stable == 0 {
                assert!(prefix.is_none());
                continue;
            }
            let path = root.join("old.json");
            fs::write(&path, &full).unwrap();
            let mut suffix = session.clone();
            suffix.messages = session.messages[stable..].to_vec();
            suffix.messages.push(message("appended"));
            let (bytes, _) = stage(&suffix);
            let mut expected = session.clone();
            expected.title = Some("a much wider final header changes the archive prefix byte offset".into());
            expected.messages.push(message("appended"));
            let mut header = expected.clone();
            header.messages.clear();
            let mut output = Vec::new();
            let moved = write_session(
                &mut output,
                &header,
                &bytes,
                Some((&path, &prefix.unwrap())),
                Some(expected.messages.len()),
                expected.messages.len(),
            )
            .unwrap()
            .unwrap();
            assert!(moved.valid(output.len() as u64, expected.messages.len()));
            assert_eq!(output, serde_json::to_vec_pretty(&expected).unwrap());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frame_counts_and_header_retention_fail_closed() {
        let mut session = Session::new("framing", Harness::Claude);
        session.messages.push(message("one"));
        let (bytes, _) = stage(&session);
        assert!(write_session(&mut Vec::new(), &session, &bytes, None, None, 1).is_err());
        session.messages.clear();
        assert!(write_session(&mut Vec::new(), &session, &bytes, None, None, 2).is_err());
        assert!(write_session(&mut Vec::new(), &session, &bytes, None, Some(2), 1).is_err());
        let (empty, _) = stage(&session);
        let mut output = Vec::new();
        assert!(write_session(&mut output, &session, &empty, None, None, 0)
            .unwrap()
            .is_none());
        assert_eq!(output, serde_json::to_vec_pretty(&session).unwrap());
    }

    #[test]
    fn bounded_label_matches_the_session_label_and_unicode_truncation() {
        for text in [
            "".into(),
            " \n\t".into(),
            "🦀".repeat(80),
            format!("{}x", " ".repeat(100_000)),
        ] {
            let mut session = Session::new("label", Harness::Claude);
            session.messages = vec![message(&text), message("later user")];
            session.messages[0].content.push(Block::Text { text: "".into() });
            let (_, first) = stage(&session);
            let first = first.unwrap();
            assert!(first.head.as_ref().unwrap().chars().count() <= 80);
            assert_eq!(cv_core::label_from(None, first.head.as_deref()), session.label());
        }
    }
}
