//! The one message window: `cv show --json` (and so `cv mcp`'s `show`), `cv export`'s JSON
//! forms and `GET /api/session/{h}/{id}/messages` all select messages through this sink. The
//! window is `[start, end)`, or the last `n`, cut at a byte budget; selected messages are staged
//! as RS-framed compact JSON ([`crate::projection::write_frame`]) and out-of-window messages pass
//! by as lazy handles, never materialized.
use crate::bytes::{ByteSource, ByteStage};
use crate::source::ReadSet;
use crate::stream::{Checkpoint, ExtraProjection, Flow, MessageSink};
use crate::{Message, Resolver, Session};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub struct WindowSink {
    resolver: Resolver,
    primary: PathBuf,
    /// Index of the next message to arrive.
    pub idx: usize,
    pub start: usize,
    /// Exclusive window end; `None` streams to EOF.
    pub end: Option<usize>,
    /// `--last n`: the ring of the last `n` messages, replayed through the window at the end.
    tail: Option<(usize, VecDeque<Message>)>,
    stage: Option<BufWriter<ByteStage>>,
    budget: Option<usize>,
    bytes: usize,
    pub selected: usize,
    /// The first in-window message the budget rejected: the continuation point.
    pub next: Option<usize>,
    /// Stop at `end` after noting a message exists there (`more`), instead of reading on to EOF
    /// for session facts written later (the `/messages` route pages; the CLI reads whole).
    stop_at_end: bool,
    pub more: bool,
    /// Keep the selected messages materialized too (redaction needs the decoded tree).
    kept: Option<Vec<Message>>,
    /// Session metadata the stream delivered (codex meta / the parse-bridge), when any did.
    pub(super) meta: Option<Value>,
    fail: Option<anyhow::Error>,
    pub(super) capture: bool,
    pub(super) checkpoint: Option<(usize, Checkpoint, Option<Value>)>,
    pub(super) reads: ReadSet,
}

impl WindowSink {
    /// A window over `primary`'s session, read to EOF, frames staged.
    pub fn new(primary: &Path, start: usize, end: Option<usize>) -> Self {
        Self {
            resolver: Resolver::new(Some(primary.to_path_buf())),
            primary: primary.to_path_buf(),
            idx: 0,
            start,
            end,
            tail: None,
            stage: Some(BufWriter::with_capacity(64 * 1024, ByteStage::default())),
            budget: None,
            bytes: 0,
            selected: 0,
            next: None,
            stop_at_end: false,
            more: false,
            kept: None,
            meta: None,
            fail: None,
            capture: false,
            checkpoint: None,
            reads: ReadSet::default(),
        }
    }

    /// The API's page: stops at `end`, may capture a checkpoint there for the next page.
    pub(super) fn page(primary: &Path, start: usize, end: Option<usize>, capture: bool, reads: ReadSet) -> Self {
        Self {
            stop_at_end: true,
            capture,
            reads,
            ..Self::new(primary, start, end)
        }
    }

    /// Select the last `n` messages instead of `[start, end)`.
    pub fn tail(mut self, n: usize) -> Self {
        self.tail = Some((n, VecDeque::new()));
        self
    }

    /// Stop before the in-window message that would push the staged bytes past `budget`; the
    /// first in-window message always fits.
    pub fn budget(mut self, budget: Option<usize>) -> Self {
        self.budget = budget;
        self
    }

    /// Materialize and keep the selected messages as well.
    pub fn keeping(mut self) -> Self {
        self.kept = Some(Vec::new());
        self
    }

    fn accept(&mut self, mut message: Message, index: usize) -> Result<()> {
        if self.next.is_some() {
            return Ok(());
        }
        let remaining = (self.selected > 0)
            .then(|| self.budget.map(|budget| budget.saturating_sub(self.bytes)))
            .flatten();
        let stage = self.stage.as_mut().context("window frames already taken")?;
        // Pending bytes belong to the same spool; do not flush each frame.
        let before = stage.get_ref().len() + stage.buffer().len() as u64;
        let size = match self.kept.as_mut() {
            None => crate::projection::write_frame(stage, &message, &self.resolver, remaining)?,
            Some(_) => {
                message.materialize(&self.resolver)?;
                crate::projection::write_frame(&mut std::io::sink(), &message, &self.resolver, remaining)?
            }
        };
        if self.selected > 0
            && self
                .budget
                .is_some_and(|budget| size > budget.saturating_sub(self.bytes))
        {
            stage.flush()?;
            stage.get_mut().truncate(before)?;
            self.next = Some(index);
            return Ok(());
        }
        self.bytes = self.bytes.checked_add(size).context("JSON budget size overflow")?;
        self.selected += 1;
        if let Some(kept) = self.kept.as_mut() {
            kept.push(message);
        }
        Ok(())
    }

    /// Replay a kept tail through the window and hand over the staged frames. The sink's counts
    /// (`selected`, `next`, `more`, `idx`) stay readable.
    pub fn finish(&mut self) -> Result<ByteSource> {
        if let Some(error) = self.fail.take() {
            return Err(error);
        }
        if let Some((_, ring)) = self.tail.take() {
            self.start = self.idx - ring.len();
            for (i, message) in ring.into_iter().enumerate() {
                self.accept(message, self.start + i)?;
            }
        }
        let stage = self.stage.take().context("window frames already taken")?;
        Ok(stage.into_inner().map_err(|error| error.into_error())?.finish()?)
    }

    /// The materialized selection of a [`Self::keeping`] window.
    pub fn take_kept(&mut self) -> Vec<Message> {
        self.kept.take().unwrap_or_default()
    }

    /// The staged frames as values (the unstaged `/messages` reply).
    pub(super) fn values(&mut self) -> Result<Vec<Value>> {
        let frames = self.finish()?;
        let mut bytes = Vec::new();
        frames.copy_to(&mut bytes)?;
        bytes
            .split(|byte| *byte == 0x1e)
            .filter(|frame| !frame.is_empty())
            .map(|frame| serde_json::from_slice(frame).map_err(Into::into))
            .collect()
    }

    fn in_window(&self, index: usize) -> bool {
        index >= self.start && self.end.is_none_or(|end| index < end)
    }
}

impl MessageSink for WindowSink {
    fn extra_projection(&self) -> ExtraProjection {
        if self.tail.is_some() || (self.fail.is_none() && self.next.is_none() && self.in_window(self.idx)) {
            ExtraProjection::Full
        } else {
            ExtraProjection::Facts
        }
    }
    fn dependency(&mut self, path: &Path) {
        self.reads.record(path);
    }
    fn wants_checkpoint(&mut self, _: u64) -> bool {
        self.capture && self.end.is_some_and(|end| end > self.start && self.idx == end)
    }
    fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
        self.capture = false;
        if super::read::bounded_size(&(&*checkpoint, &self.meta)).is_some() {
            self.checkpoint = Some((self.idx, checkpoint.clone(), self.meta.clone()));
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.fail.take()
    }
    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(super::session_meta_json(s));
        }
        if s.source_path.is_some() {
            self.resolver = s.resolver();
        }
    }
    fn message(&mut self, m: Message) -> Flow {
        crate::source::record_message_reads(&m, &self.primary, &mut self.reads);
        let index = self.idx;
        self.idx += 1;
        if self.fail.is_some() {
            return Flow::Stop;
        }
        if let Some((n, ring)) = self.tail.as_mut() {
            if *n > 0 {
                ring.push_back(m);
                if ring.len() > *n {
                    ring.pop_front();
                }
            }
            return Flow::Continue;
        }
        if index < self.start {
            return Flow::Continue; // before the window: skip without touching content bytes
        }
        if self.end.is_some_and(|end| index >= end) {
            if self.stop_at_end {
                self.more = true; // a message exists at `end`: note it, read no further
                return Flow::Stop;
            }
            return Flow::Continue; // past the window: session facts written later still arrive
        }
        match self.accept(m, index) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.fail = Some(error);
                Flow::Stop
            }
        }
    }
}
