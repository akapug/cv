//! Strict read-only JSONL projection, sharing document source slices and navigation indexes.

pub(crate) mod ranges;

use super::document;
use std::fs::File;
use std::path::Path;

/// One physical JSONL line, as the record reader hands it to a codec.
pub(crate) struct Line<'v> {
    /// Absolute offset of the record after ASCII whitespace trimming.
    pub at: u64,
    /// Absolute offset just past the line: after its LF, or the end of the read.
    pub end: u64,
    /// The writer finished this line (it ends in LF); only such boundaries may checkpoint.
    pub complete: bool,
    /// The Value-compatible record, or None for a blank, non-UTF-8 or rejected line.
    pub view: Option<crate::json::JsonView<'v>>,
    /// Nothing but ASCII whitespace: a `None` view on any other line is a rejected record.
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
/// Records are trimmed of ASCII whitespace only, as JSON itself allows. Read errors end the
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
    if let Ok(map) = unsafe { memmap2::Mmap::map(file) } {
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let bytes = usize::try_from(from)
            .ok()
            .zip(usize::try_from(to).ok())
            .and_then(|(from, to)| map.get(from..to))
            .unwrap_or(&[]);
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', bytes).map(|i| i + 1) {
            let line = &bytes[start..end - 1];
            let (at, end) = (from + start as u64, from + end as u64);
            if in_memory(source, decoded, at, end, true, line, &mut visit)? == Flow::Stop {
                return Ok(());
            }
            start = end as usize - from as usize;
        }
        if start < bytes.len() {
            in_memory(
                source,
                decoded,
                from + start as u64,
                to,
                false,
                &bytes[start..],
                &mut visit,
            )?;
        }
        return Ok(());
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
            let range = trim_ascii(file, raw)?;
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
                None => source.read_positioned_range_value(&context, at, range.len, &mut view),
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

/// Records a first [`for_each_line`] pass kept by range (a line's, or a value inside one, from
/// `JsonView::source_range`), read again from the same descriptor in any order: for formats
/// whose later records replace earlier ones, so selection must reach EOF before decoding.
/// Mapped builds borrow every range from one mapping; positioned builds read a small range into
/// one buffer and leave a giant one in place, exactly as [`for_each_line`] does.
pub(crate) struct Records<'s> {
    source: &'s crate::json::JsonSource,
    file: &'s File,
    #[cfg(feature = "mmap")]
    map: Option<memmap2::Mmap>,
    context: Option<document::range::RangeSource>,
    buffer: Vec<u8>,
}

impl<'s> Records<'s> {
    pub(crate) fn new(source: &'s crate::json::JsonSource, file: &'s File) -> Self {
        Self {
            source,
            file,
            #[cfg(feature = "mmap")]
            map: unsafe { memmap2::Mmap::map(file) }.ok(),
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
        #[cfg(feature = "mmap")]
        if let Some(map) = &self.map {
            let bytes = usize::try_from(at)
                .ok()
                .zip(usize::try_from(at + len).ok())
                .and_then(|(from, to)| map.get(from..to))
                .ok_or_else(|| anyhow::anyhow!("record range outside its source"))?;
            return self.source.read_record(at, std::str::from_utf8(bytes)?, f);
        }
        if len <= 64 * 1024 {
            self.buffer.resize(len as usize, 0);
            ranges::read_exact_at(self.file, &mut self.buffer, at)?;
            return self.source.read_record(at, std::str::from_utf8(&self.buffer)?, f);
        }
        if self.context.is_none() {
            self.context = Some(document::range::RangeSource::from_captured(self.source.clone())?);
        }
        let context = self.context.as_ref().expect("context was just made");
        self.source.read_positioned_range_value(context, at, len, f)
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
    let lead = bytes.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let tail = bytes[lead..]
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    let trimmed = &bytes[lead..bytes.len() - tail];
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

/// ASCII-trim a giant positioned record by reading only its boundary windows.
fn trim_ascii(file: &File, mut range: ranges::Range) -> anyhow::Result<ranges::Range> {
    let mut bytes = [0; 64 * 1024];
    while range.len > 0 {
        let n = range.len.min(bytes.len() as u64) as usize;
        ranges::read_exact_at(file, &mut bytes[..n], range.offset)?;
        let lead = bytes[..n].iter().take_while(|b| b.is_ascii_whitespace()).count();
        range.offset += lead as u64;
        range.len -= lead as u64;
        if lead != n {
            break;
        }
    }
    while range.len > 0 {
        let n = range.len.min(bytes.len() as u64) as usize;
        ranges::read_exact_at(file, &mut bytes[..n], range.offset + range.len - n as u64)?;
        let tail = bytes[..n].iter().rev().take_while(|b| b.is_ascii_whitespace()).count();
        range.len -= tail as u64;
        if tail != n {
            break;
        }
    }
    Ok(range)
}
