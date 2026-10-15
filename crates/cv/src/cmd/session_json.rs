//! Shared bounded pretty Session framing for archive and CLI output.
use anyhow::{Context, Result};
use cv_core::{bytes::ByteSource, Session};
use serde::{Deserialize, Serialize};
use serde_json::ser::{Formatter, PrettyFormatter};
use std::{
    cell::{Cell, RefCell},
    fs,
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
    path::Path,
    rc::Rc,
};
#[cfg(test)]
thread_local! { static FRAME_SERIALIZATIONS: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn take_frame_serializations() -> usize {
    FRAME_SERIALIZATIONS.with(|count| count.replace(0))
}
/// RS cannot occur unescaped in JSON; each independently compact message is one frame.
pub(crate) fn write_message_frame(
    writer: &mut impl Write,
    message: &cv_core::Message,
    resolver: &cv_core::Resolver,
) -> Result<()> {
    writer.write_all(&[0x1e])?;
    #[cfg(test)]
    FRAME_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    message.write_json(writer, resolver)
}

/// Count the complete compact frame while retaining only its budget-sized prefix. Discarding
/// bytes still reports successful writes, so all lazy demands and source checks run to completion.
pub(crate) fn write_budgeted_message_frame(
    stage: &mut impl Write,
    message: &cv_core::Message,
    resolver: &cv_core::Resolver,
    remaining: Option<usize>,
) -> Result<usize> {
    struct Capped<'a, W> {
        stage: &'a mut W,
        count: usize,
        cap: Option<usize>,
    }
    impl<W: Write> Write for Capped<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = self
                .cap
                .map_or(bytes.len(), |cap| bytes.len().min(cap.saturating_sub(self.count)));
            if n != 0 {
                self.stage.write_all(&bytes[..n])?;
            }
            self.count = self
                .count
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("JSON size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stage.flush()
        }
    }
    let mut writer = Capped {
        stage,
        count: 0,
        cap: remaining.map(|n| n.saturating_add(1)),
    };
    write_message_frame(&mut writer, message, resolver)?;
    writer.count.checked_sub(1).context("JSON frame omitted boundary")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Prefix {
    pub start: u64,
    pub end: u64,
    pub messages: usize,
}
impl Prefix {
    pub fn valid(&self, bytes: u64, messages: usize) -> bool {
        self.messages == messages && messages > 0 && self.start < self.end && self.end <= bytes
    }
}

pub(crate) struct Counted<W> {
    pub(crate) inner: W,
    pub(crate) position: Rc<Cell<u64>>,
}
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.position.set(
            self.position
                .get()
                .checked_add(n as u64)
                .ok_or_else(|| io::Error::other("JSON output length overflow"))?,
        );
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Preserve Session's derived field order/omission rules and serde's pretty output. Only the
/// known root messages array is replaced with already serialized values; no JSON schema parser.
struct Messages<'a> {
    pretty: PrettyFormatter<'static>,
    depth: usize,
    key: Vec<u8>,
    reading_key: bool,
    messages_key: bool,
    injected: bool,
    spool: Box<dyn Read + 'a>,
    old: Option<(&'a Path, &'a Prefix)>,
    position: Rc<Cell<u64>>,
    count: usize,
    total: usize,
    stable: Option<usize>,
    start: Option<u64>,
    prefix: Option<Prefix>,
    result: Rc<RefCell<Option<Option<Prefix>>>>,
}
impl Messages<'_> {
    fn record_end(&mut self) {
        if self.stable == Some(self.count) {
            self.prefix = self.start.map(|start| Prefix {
                start,
                end: self.position.get(),
                messages: self.count,
            });
        }
    }
    fn begin_message<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.begin_array_value(writer, self.count == 0)?;
        self.start.get_or_insert(self.position.get());
        Ok(())
    }
    fn end_message<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.count += 1;
        self.record_end();
        self.pretty.end_array_value(writer)
    }
    fn inject<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if let Some((path, prefix)) = self.old {
            self.begin_message(writer)?;
            let mut old = fs::File::open(path)?;
            old.seek(SeekFrom::Start(prefix.start))?;
            let expected = prefix.end - prefix.start;
            let copied = io::copy(&mut old.take(expected), writer)?;
            if copied != expected {
                return Err(io::Error::other("archive stable prefix truncated"));
            }
            self.count = prefix.messages;
            self.record_end();
            self.pretty.end_array_value(writer)?;
        }
        let spool = std::mem::replace(&mut self.spool, Box::new(io::empty()));
        let mut spool = io::BufReader::with_capacity(64 * 1024, spool);
        while !spool.fill_buf()?.is_empty() {
            if spool.fill_buf()?[0] != 0x1e {
                return Err(io::Error::other("JSON spool has no frame boundary"));
            }
            spool.consume(1);
            self.begin_message(writer)?;
            {
                // PrettyJson only changes whitespace of the compact frame already accepted
                // and serialized by Message. Its base includes the enclosing messages array.
                let mut output = &mut *writer;
                let mut pretty = cv_core::PrettyJson::new(&mut output, 2);
                loop {
                    let bytes = spool.fill_buf()?;
                    let end = bytes.iter().position(|byte| *byte == 0x1e).unwrap_or(bytes.len());
                    if end == 0 {
                        break;
                    }
                    pretty.write_all(&bytes[..end])?;
                    spool.consume(end);
                }
            }
            self.end_message(writer)?;
        }
        if self.count != self.total {
            return Err(io::Error::other("JSON spool message count mismatch"));
        }
        if self.stable.is_some_and(|stable| stable > 0 && self.prefix.is_none()) {
            return Err(io::Error::other("archive checkpoint has no serialized prefix"));
        }
        *self.result.borrow_mut() = Some(self.prefix.clone());
        Ok(())
    }
}
impl Formatter for Messages<'_> {
    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        let inject = self.depth == 1 && self.messages_key;
        self.depth += 1;
        self.pretty.begin_array(w)?;
        if inject {
            if self.injected {
                return Err(io::Error::other("duplicate Session messages array"));
            }
            self.injected = true;
            self.inject(w)?;
        }
        Ok(())
    }
    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth -= 1;
        self.pretty.end_array(w)
    }
    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth += 1;
        self.pretty.begin_object(w)
    }
    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth -= 1;
        self.pretty.end_object(w)
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.reading_key = self.depth == 1;
        self.key.clear();
        self.pretty.begin_object_key(w, first)
    }
    fn write_string_fragment<W: ?Sized + Write>(&mut self, w: &mut W, s: &str) -> io::Result<()> {
        if self.reading_key {
            if self.key.len().saturating_add(s.len()) <= 8 {
                self.key.extend_from_slice(s.as_bytes());
            } else {
                self.reading_key = false;
            }
        }
        self.pretty.write_string_fragment(w, s)
    }
    fn end_object_key<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.messages_key = self.reading_key && self.key == b"messages";
        self.reading_key = false;
        self.pretty.end_object_key(w)
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_object_value(w)
    }
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.pretty.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_array_value(w)
    }
}

pub(crate) fn write_session(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    old: Option<(&Path, &Prefix)>,
    stable: Option<usize>,
    total: usize,
) -> Result<Option<Prefix>> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    write_framed(writer, session, spool, old, stable, total)
}

pub(crate) fn write_document(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    total: usize,
    document: bool,
) -> Result<()> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    if document {
        write_framed(
            writer,
            &cv_core::harness::opensession::document(session),
            spool,
            None,
            None,
            total,
        )?;
    } else {
        write_framed(writer, session, spool, None, None, total)?;
    }
    Ok(())
}

fn write_framed(
    writer: &mut impl Write,
    session: &impl Serialize,
    spool: &ByteSource,
    old: Option<(&Path, &Prefix)>,
    stable: Option<usize>,
    total: usize,
) -> Result<Option<Prefix>> {
    let position = Rc::new(Cell::new(0));
    let result = Rc::new(RefCell::new(None));
    let formatter = Messages {
        pretty: PrettyFormatter::new(),
        depth: 0,
        key: Vec::with_capacity(8),
        reading_key: false,
        messages_key: false,
        injected: false,
        spool: spool.reader()?,
        old,
        position: position.clone(),
        count: 0,
        total,
        stable,
        start: None,
        prefix: None,
        result: result.clone(),
    };
    let mut counted = Counted {
        inner: writer,
        position,
    };
    let mut serializer = serde_json::Serializer::with_formatter(&mut counted, formatter);
    session.serialize(&mut serializer)?;
    drop(serializer);
    let prefix = result
        .borrow_mut()
        .take()
        .context("Session serializer omitted messages")?;
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Calls {
        bytes: Vec<u8>,
        writes: usize,
    }
    impl Write for Calls {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn persistent_stage_buffers_batch_structural_frames_and_document_tokens() {
        let mut message = cv_core::Message::new(cv_core::Role::Assistant);
        message.content.push(cv_core::Block::ToolUse {
            id: "call".into(), name: "tool".into(), namespace: None,
            input: serde_json::json!({"rows": (0..128).map(|n| serde_json::json!({"n": n, "s": "x\n"})).collect::<Vec<_>>()}).into(),
        });
        let resolver = cv_core::Resolver::new(None);
        let mut plain = Calls::default();
        let mut buffered = io::BufWriter::with_capacity(64 * 1024, Calls::default());
        for _ in 0..128 {
            write_budgeted_message_frame(&mut plain, &message, &resolver, None).unwrap();
            write_budgeted_message_frame(&mut buffered, &message, &resolver, None).unwrap();
        }
        let buffered = buffered.into_inner().map_err(|e| e.into_error()).unwrap();
        assert_eq!(buffered.bytes, plain.bytes);
        assert!(buffered.writes < plain.writes / 100);
        assert!(buffered.writes <= plain.bytes.len().div_ceil(64 * 1024) + 1);
        let mut stage = cv_core::bytes::ByteStage::new();
        stage.write_all(&plain.bytes).unwrap();
        let spool = stage.finish().unwrap();
        let session = Session::new("buffered", cv_core::Harness::Codex);
        let mut plain = Calls::default();
        let mut buffered = io::BufWriter::with_capacity(64 * 1024, Calls::default());
        write_document(&mut plain, &session, &spool, 128, true).unwrap();
        write_document(&mut buffered, &session, &spool, 128, true).unwrap();
        let buffered = buffered.into_inner().map_err(|e| e.into_error()).unwrap();
        assert_eq!(buffered.bytes, plain.bytes);
        assert!(buffered.writes < plain.writes / 100);
        eprintln!(
            "document writes {} -> {} for {} bytes",
            plain.writes,
            buffered.writes,
            plain.bytes.len()
        );
    }

    #[test]
    fn buffered_stage_finish_reports_deferred_write_errors() {
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Other.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = io::BufWriter::with_capacity(64 * 1024, Failed);
        writer.write_all(b"pending frame").unwrap();
        assert!(writer.flush().is_err());
        assert!(writer.into_inner().is_err());
    }

    #[test]
    fn rejected_giant_frames_stage_only_remaining_budget_and_still_demand_late_sources() {
        let world = crate::test_world("capped-session-frame");
        let resolver = cv_core::Resolver::new(None);
        let mut first = cv_core::Message::new(cv_core::Role::User);
        first.content.push(cv_core::Block::Text { text: "tiny".into() });
        let mut giant = cv_core::Message::new(cv_core::Role::Assistant);
        giant.content.push(cv_core::Block::Text {
            text: "giant\n🌙".repeat(1024 * 1024).into(),
        });
        let mut stage = io::BufWriter::with_capacity(64 * 1024, cv_core::bytes::ByteStage::new());
        let first_len = write_budgeted_message_frame(&mut stage, &first, &resolver, None).unwrap();
        let accepted = stage.get_ref().len() + stage.buffer().len() as u64;
        let remaining = 7;
        let size = write_budgeted_message_frame(&mut stage, &giant, &resolver, Some(remaining)).unwrap();
        assert!(size > remaining);
        // Stage writes only append here: the final extent is also its peak speculative extent.
        assert_eq!(
            stage.get_ref().len() + stage.buffer().len() as u64,
            accepted + remaining as u64 + 1
        );
        stage.flush().unwrap();
        stage.get_mut().truncate(accepted).unwrap();
        let mut session = Session::new("capped", cv_core::Harness::Codex);
        let spool = stage
            .into_inner()
            .map_err(|e| e.into_error())
            .unwrap()
            .finish()
            .unwrap();
        let mut output = Vec::new();
        write_session(&mut output, &session, &spool, None, None, 1).unwrap();
        session.messages.push(first);
        assert_eq!(output, serde_json::to_vec_pretty(&session).unwrap());
        assert_eq!(first_len + 1, accepted as usize);

        let path = world.base.join("late.json");
        fs::write(&path, b"{\"late\":true}").unwrap();
        let input = cv_core::Json::source(&path, 0, fs::metadata(&path).unwrap().len()).unwrap();
        giant.content.push(cv_core::Block::ToolUse {
            id: "late".into(),
            name: "tool".into(),
            namespace: None,
            input,
        });
        fs::remove_file(&path).unwrap();
        let mut stage = cv_core::bytes::ByteStage::new();
        assert!(write_budgeted_message_frame(&mut stage, &giant, &resolver, Some(remaining)).is_err());
        assert_eq!(stage.len(), remaining as u64 + 1);
    }

    #[test]
    fn frame_stage_contains_exactly_one_compact_serialization() {
        let mut message = cv_core::Message::new(cv_core::Role::Assistant);
        message.content.push(cv_core::Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            namespace: None,
            input: serde_json::json!({"quoted": "🌙\n\\\"", "nested": [{"a": []}, true, 12]}).into(),
        });
        let expected = serde_json::to_vec(&message).unwrap();
        let mut stage = cv_core::bytes::ByteStage::new();
        take_frame_serializations();
        write_message_frame(&mut stage, &message, &cv_core::Resolver::new(None)).unwrap();
        assert_eq!(take_frame_serializations(), 1);
        assert_eq!(stage.len(), expected.len() as u64 + 1);
        let pretty_frame = serde_json::to_vec_pretty(&message).unwrap().len() + 1;
        assert!(stage.len() < pretty_frame as u64);
        let mut actual = Vec::new();
        stage.finish().unwrap().copy_to(&mut actual).unwrap();
        assert_eq!(actual[0], 0x1e);
        assert_eq!(actual[1..], expected);
    }
}
