//! Strict read-only JSONL projection, sharing document source slices and navigation indexes.

pub(crate) mod ranges;

use super::document;
use std::fs::File;
use std::path::Path;

/// One physical JSONL line, as the record reader hands it to a codec.
pub(crate) struct Line<'v> {
    /// Absolute offset of the record after JSON whitespace trimming.
    pub at: u64,
    /// Absolute offset just past the line: after its LF, or the end of the read.
    pub end: u64,
    /// The writer finished this line (it ends in LF); only such boundaries may checkpoint.
    pub complete: bool,
    /// The Value-compatible record, or None for a blank, non-UTF-8 or rejected line.
    pub view: Option<crate::json::JsonView<'v>>,
    /// Nothing but JSON whitespace: a `None` view on any other line is a rejected record.
    pub blank: bool,
}

/// Every record of a whole JSONL file through [`for_each_line`], for sidecar and discovery
/// reducers that keep no offsets: the file is captured, opened against the capture, and its
/// identity rechecked at the end. Blank and rejected lines are skipped.
pub(crate) fn for_each_record(
    path: &Path,
    mut visit: impl FnMut(crate::json::JsonView<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let source = crate::json::JsonSource::capture(path)?;
    let file = File::open(path)?;
    source.check_metadata(file.metadata()?)?;
    for_each_line(&source, &file, 0, source.len(), |line| {
        if let Some(view) = line.view {
            visit(view)?;
        }
        Ok(crate::stream::Flow::Continue)
    })?;
    source.check_metadata(file.metadata()?)?;
    source.check()
}

/// Records of `from..to` in a captured JSONL source, each physical line once. This is the byte
/// source every JSONL codec shares; it is the only place the two builds differ. Mapped builds
/// borrow each line from one mapping. Positioned builds (and a failed mapping) read each small
/// line into one reused buffer and leave a giant line in place as a positioned view, so no
/// private copy of it is ever made. Either way the codec sees the same [`JsonView`] API.
///
/// Records are trimmed of JSON whitespace only (space, tab, LF, CR). Read errors end the
/// visit with an error. `file` must be the descriptor the caller checked against `source`.
pub(crate) fn for_each_line(
    source: &crate::json::JsonSource,
    file: &File,
    from: u64,
    to: u64,
    visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    lines(source, file, None, from, to, visit)
}

/// [`for_each_line`] over the source's private decoded bytes (a decompressed copy, `len` bytes
/// long, at the start of `backing`): views keep the original's provenance, offsets are decoded.
pub(crate) fn for_each_decoded_line(
    source: &crate::json::JsonSource,
    backing: &std::sync::Arc<super::bytes::ByteSource>,
    len: u64,
    from: u64,
    visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    lines(source, &backing.file()?, Some((backing, len)), from, len, visit)
}

type Decoded<'b> = Option<(&'b std::sync::Arc<super::bytes::ByteSource>, u64)>;

fn lines(
    source: &crate::json::JsonSource,
    file: &File,
    decoded: Decoded<'_>,
    from: u64,
    to: u64,
    mut visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    use crate::stream::Flow;
    #[cfg(feature = "mmap")]
    if let Some(mapped) = mapped_lines(source, file, decoded, from, to, &mut visit) {
        return mapped;
    }
    let mut frames = ranges::records(
        file,
        ranges::Range {
            offset: from,
            len: to.saturating_sub(from),
        },
    )?;
    frames.keep_small_records(64 * 1024);
    let context = match decoded {
        Some((backing, _)) => document::range::RangeSource::decoded(source.clone(), backing)?,
        None => document::range::RangeSource::from_captured(source.clone())?,
    };
    while let Some(frame) = frames.next() {
        let (raw, complete) = frame?;
        let end = frames.offset();
        let flow = if let Some(bytes) = frames.small_record() {
            in_memory(source, decoded, raw.offset, end, complete, bytes, &mut visit)?
        } else {
            let range = ranges::trim(file, raw)?;
            let at = range.offset;
            let mut visited = false;
            let blank = range.len == 0;
            let mut view = |view: crate::json::JsonView<'_>| {
                visited = true;
                visit(Line {
                    at,
                    end,
                    complete,
                    view: Some(view),
                    blank,
                })
            };
            let read = (!blank).then(|| match decoded {
                Some((backing, len)) => {
                    source.read_positioned_decoded_value(&context, backing, len, at, range.len, &mut view)
                }
                None => {
                    source.read_positioned_range(&context, at, range.len, crate::json::Validation::Value, &mut view)
                }
            });
            match read {
                Some(Err(error)) if !visited && error.downcast_ref::<crate::json::InvalidJson>().is_some() => None,
                Some(flow) => Some(flow?),
                None => None,
            }
            .map_or_else(
                || {
                    visit(Line {
                        at,
                        end,
                        complete,
                        view: None,
                        blank,
                    })
                },
                Ok,
            )?
        };
        if flow == Flow::Stop {
            break;
        }
    }
    Ok(())
}

/// The most a line pass keeps mapped at once: one window, grown only to hold a line longer than
/// it. A pass over a multi-GB transcript never holds the whole file mapped, so its resident (and,
/// on Linux, cgroup-charged) mapped bytes stay bounded by the passes running, not by file size.
/// 8 MiB in production (64 KiB in tests).
#[cfg(feature = "mmap")]
pub(crate) const MAP_WINDOW: u64 = if cfg!(test) { 64 << 10 } else { 8 << 20 };

// Tests: the largest mapping each thread has made, the bound the mapped-bytes test asserts.
#[cfg(all(test, feature = "mmap"))]
thread_local! { pub(crate) static MAPPED_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(all(test, feature = "mmap"))]
pub(crate) fn note_mapped(len: usize) {
    MAPPED_PEAK.with(|peak| peak.set(peak.get().max(len)));
}

/// [`lines`] over windows of a mapping, each dropped before the next is mapped. `None` when the
/// first window cannot be mapped: the caller reads positioned instead.
#[cfg(feature = "mmap")]
fn mapped_lines(
    source: &crate::json::JsonSource,
    file: &File,
    decoded: Decoded<'_>,
    from: u64,
    to: u64,
    visit: &mut impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> Option<anyhow::Result<()>> {
    use crate::stream::Flow;
    let mut at = from;
    while at < to {
        // A window's complete lines end at its last LF; one holding none grows until it does.
        let mut end = to.min(at.saturating_add(MAP_WINDOW));
        let map = loop {
            let len = usize::try_from(end - at).ok()?;
            let map = match unsafe { memmap2::MmapOptions::new().offset(at).len(len).map(file) } {
                Ok(map) => map,
                Err(_) if at == from => return None,
                Err(error) => return Some(Err(error.into())),
            };
            if end == to || memchr::memrchr(b'\n', &map).is_some() {
                break map;
            }
            end = to.min(at.saturating_add(2 * (end - at)));
        };
        #[cfg(test)]
        note_mapped(map.len());
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let last = match end == to {
            true => map.len(),
            false => memchr::memrchr(b'\n', &map).map_or(map.len(), |i| i + 1),
        };
        let bytes = &map[..last];
        let mut start = 0;
        for stop in memchr::memchr_iter(b'\n', bytes).map(|i| i + 1) {
            let line = &bytes[start..stop - 1];
            let flow = in_memory(source, decoded, at + start as u64, at + stop as u64, true, line, visit);
            match flow {
                Ok(Flow::Stop) => return Some(Ok(())),
                Err(error) => return Some(Err(error)),
                Ok(Flow::Continue) => {}
            }
            start = stop;
        }
        if start < bytes.len() {
            // Only the read's last window can end without an LF: a partial last line.
            return Some(in_memory(source, decoded, at + start as u64, to, false, &bytes[start..], visit).map(|_| ()));
        }
        at += last as u64;
    }
    Some(Ok(()))
}

/// Records a first [`for_each_line`] pass kept by range (a line's, or a value inside one, from
/// `JsonView::source_range`), read again from the same descriptor in any order: for formats
/// whose later records replace earlier ones, so selection must reach EOF before decoding.
/// A small range is read into one buffer. A giant one is mapped for its read alone in mapped
/// builds and left in place as a positioned view otherwise, so the file is never held mapped.
/// A record that is not UTF-8 is rejected ([`InvalidJson`](crate::json::InvalidJson)), as
/// [`for_each_line`] rejects such a line.
pub(crate) struct Records<'s> {
    source: &'s crate::json::JsonSource,
    file: &'s File,
    context: Option<document::range::RangeSource<'static>>,
    buffer: Vec<u8>,
}

impl<'s> Records<'s> {
    pub(crate) fn new(source: &'s crate::json::JsonSource, file: &'s File) -> Self {
        Self {
            source,
            file,
            context: None,
            buffer: Vec::new(),
        }
    }

    pub(crate) fn read<T>(
        &mut self,
        at: u64,
        len: u64,
        f: impl FnOnce(crate::json::JsonView<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.read_with(at, len, crate::json::Validation::Value, f)
    }
    /// [`Self::read`] under a chosen acceptance.
    pub(crate) fn read_with<T>(
        &mut self,
        at: u64,
        len: u64,
        validation: crate::json::Validation,
        f: impl FnOnce(crate::json::JsonView<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(
            at.checked_add(len).is_some_and(|end| end <= self.source.len()),
            "record range outside its source"
        );
        #[cfg(feature = "mmap")]
        if len > 64 * 1024 {
            // SAFETY: a read-only mapping of the checked descriptor, dropped with this read.
            let map = unsafe {
                memmap2::MmapOptions::new()
                    .offset(at)
                    .len(usize::try_from(len)?)
                    .map(self.file)?
            };
            #[cfg(test)]
            note_mapped(map.len());
            let text = std::str::from_utf8(&map).map_err(|_| crate::json::InvalidJson)?;
            return self.source.read_record_with(at, text, validation, f);
        }
        if len <= 64 * 1024 {
            self.buffer.resize(len as usize, 0);
            ranges::read_exact_at(self.file, &mut self.buffer, at)?;
            let text = std::str::from_utf8(&self.buffer).map_err(|_| crate::json::InvalidJson)?;
            return self.source.read_record_with(at, text, validation, f);
        }
        if self.context.is_none() {
            self.context = Some(document::range::RangeSource::from_captured(self.source.clone())?);
        }
        let context = self.context.as_ref().expect("context was just made");
        self.source.read_positioned_range(context, at, len, validation, f)
    }
}

/// One line whose bytes are in memory (mapped, or read as a small positioned record).
fn in_memory(
    source: &crate::json::JsonSource,
    decoded: Decoded<'_>,
    start: u64,
    end: u64,
    complete: bool,
    bytes: &[u8],
    visit: &mut impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<crate::stream::Flow> {
    let lead = bytes.iter().take_while(|b| super::json::is_whitespace(b)).count();
    let trimmed = super::json::trim(&bytes[lead..]);
    let at = start + lead as u64;
    let blank = trimmed.is_empty();
    if let Some(text) = (!blank).then(|| std::str::from_utf8(trimmed).ok()).flatten() {
        let mut visited = false;
        let view = |view: crate::json::JsonView<'_>| {
            visited = true;
            visit(Line {
                at,
                end,
                complete,
                view: Some(view),
                blank,
            })
        };
        let read = match decoded {
            Some((backing, len)) => source.read_decoded_record(backing, len, at, text, view),
            None => source.read_record(at, text, view),
        };
        match read {
            Err(error) if !visited && error.downcast_ref::<crate::json::InvalidJson>().is_some() => {}
            flow => return flow,
        }
    }
    visit(Line {
        at,
        end,
        complete,
        view: None,
        blank,
    })
}

#[cfg(test)]
mod tests {
    use crate::stream::Flow;

    /// JSON whitespace is space, tab, LF and CR. A line holding only U+2003 (EM SPACE), short or
    /// past the in-memory limit, is a rejected record to every reader of a transcript, and a line
    /// of JSON whitespace is blank to all of them.
    #[test]
    fn every_path_trims_json_whitespace_only() {
        let world = cv_testkit::World::new("jsonl-json-whitespace");
        let record =
            r#"{"type":"user","uuid":"a","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"hi"}}"#;
        for (name, filler, blank) in [
            ("em", "\u{2003}".to_owned(), false),
            ("em-large", "\u{2003}".repeat(40_000), false),
            ("ascii", " \t\r".to_owned(), true),
            ("ascii-large", " \t".repeat(70_000), true),
        ] {
            let path = world.base.join(format!("{name}.jsonl"));
            let text = format!("{record}\n{filler}\n");
            std::fs::write(&path, &text).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();

            let mut codec = Vec::new();
            super::for_each_line(&source, &file, 0, source.len(), |line| {
                codec.push((line.blank, line.view.is_some()));
                Ok(Flow::Continue)
            })
            .unwrap();
            assert_eq!(codec, [(false, true), (blank, false)], "codec, {name}");

            let lines = 1 + usize::from(!blank);
            let counts = crate::restamp::write_restamped_transcript(&source, &file, "n", &mut std::io::sink()).unwrap();
            assert_eq!(counts.lines, lines, "restamp, {name}");
            assert_eq!(crate::restamp::restamp(&text, "n").lines, lines, "restamp text, {name}");
            assert_eq!(
                crate::harness::for_each_json_line_str(&text, |_| Flow::Continue),
                u64::from(!blank),
                "line reader, {name}"
            );

            let verified = crate::harness::claude::emit::verify_claude_resumable(&path, None);
            assert_eq!(verified.is_ok(), blank, "verify, {name}: {verified:?}");

            let opts = crate::prune::PruneOptions {
                new_id: Some(format!("{name}-pruned")),
                ..Default::default()
            };
            let pruned = crate::prune::prune_session(&path, &opts).unwrap();
            let output = std::fs::read_to_string(&pruned.new_path).unwrap();
            assert_eq!(output.contains('\u{2003}'), !blank, "prune, {name}");
            assert_eq!(output.lines().count(), lines, "prune, {name}");
        }
    }

    /// A writer appending half records at a time while passes read: every pass reads to its
    /// captured length without error, each record it yields is whole, a trailing partial line is
    /// held back (no view, not complete), and the source still checks at the end. No sleeps.
    #[test]
    fn a_pass_beside_a_writer_of_half_records_never_errors_or_yields_a_partial() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        let world = cv_testkit::World::new("jsonl-live-writer");
        let path = world.base.join("live.jsonl");
        let record = |i: usize| format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "p".repeat(i % 300));
        std::fs::write(&path, (0..200).map(record).collect::<String>()).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                for i in 200..20_000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let line = record(i);
                    let (head, tail) = line.split_at(line.len() / 2);
                    file.write_all(head.as_bytes()).unwrap();
                    file.write_all(tail.as_bytes()).unwrap();
                }
            });
            for _ in 0..20 {
                let source = crate::json::JsonSource::capture(&path).unwrap();
                let file = std::fs::File::open(&path).unwrap();
                source.check_metadata(file.metadata().unwrap()).unwrap();
                let mut records = Vec::new();
                super::for_each_line(&source, &file, 0, source.len(), |line| {
                    match line.view {
                        Some(view) => {
                            assert!(line.complete, "a record is whole");
                            records.push(view.source_range()?.unwrap());
                        }
                        None => assert!(!line.complete && line.end == source.len(), "only a held-back tail"),
                    }
                    Ok(Flow::Continue)
                })
                .unwrap();
                let bytes = std::fs::read(&path).unwrap();
                for (n, (at, len)) in records.into_iter().enumerate() {
                    assert_eq!(
                        &bytes[at as usize..(at + len) as usize],
                        record(n).trim_end().as_bytes()
                    );
                }
                source.check_metadata(file.metadata().unwrap()).unwrap();
                source.check().unwrap();
            }
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
        });
    }
}
