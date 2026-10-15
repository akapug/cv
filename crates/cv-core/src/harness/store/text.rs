//! Bounded text construction in a checked shared source arena.
use super::bytes::ByteSource;
use crate::{
    json::{JsonSource, JsonView},
    lazy::Text,
};
use anyhow::Result;
use serde::Serialize;
use std::{io::Write, sync::Arc};

pub(crate) struct Builder<'a>(&'a mut dyn Write, usize);

// Count canonical encoded string interiors without allocating decoded bytes. The canonical
// writer has already checked strict string validity; escape state survives fragmented writes.
#[derive(Default)]
struct StringCount {
    len: usize,
    escape: bool,
    unicode: u8,
    scalar: u16,
    high: bool,
}
impl StringCount {
    fn add(&mut self, bytes: usize) -> std::io::Result<()> {
        self.len = self
            .len
            .checked_add(bytes)
            .ok_or_else(|| std::io::Error::other("text length overflow"))?;
        Ok(())
    }
    fn feed(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        while let Some((&byte, rest)) = bytes.split_first() {
            if self.unicode != 0 {
                let digit = (byte as char)
                    .to_digit(16)
                    .ok_or_else(|| std::io::Error::other("invalid unicode escape"))?;
                self.scalar = (self.scalar << 4) | digit as u16;
                self.unicode -= 1;
                bytes = rest;
                if self.unicode == 0 {
                    match self.scalar {
                        0xd800..=0xdbff if !self.high => self.high = true,
                        0xdc00..=0xdfff if self.high => {
                            self.high = false;
                            self.add(4)?;
                        }
                        0xd800..=0xdfff => return Err(std::io::Error::other("invalid surrogate")),
                        scalar if !self.high => self.add(if scalar < 0x80 {
                            1
                        } else if scalar < 0x800 {
                            2
                        } else {
                            3
                        })?,
                        _ => return Err(std::io::Error::other("missing low surrogate")),
                    }
                }
            } else if self.escape {
                self.escape = false;
                bytes = rest;
                if byte == b'u' {
                    self.unicode = 4;
                    self.scalar = 0;
                } else {
                    if self.high {
                        return Err(std::io::Error::other("missing low surrogate"));
                    }
                    self.add(1)?;
                }
            } else {
                let end = memchr::memchr(b'\\', bytes).unwrap_or(bytes.len());
                if self.high && end != 0 {
                    return Err(std::io::Error::other("missing low surrogate"));
                }
                self.add(end)?;
                bytes = &bytes[end..];
                if !bytes.is_empty() {
                    self.escape = true;
                    bytes = &bytes[1..];
                }
            }
        }
        Ok(())
    }
    fn finish(self) -> std::io::Result<usize> {
        if self.escape || self.unicode != 0 || self.high {
            return Err(std::io::Error::other("incomplete string escape"));
        }
        Ok(self.len)
    }
}
impl Builder<'_> {
    pub(crate) fn decoded_len(&self) -> usize {
        self.1
    }
    /// Append decoded text with serde's exact escapes, streaming through the caller's writer.
    /// Arena appends already protect long borrowed writes with bounded independently owned scratch.
    pub(crate) fn push(&mut self, text: &str) -> Result<()> {
        struct Interior;
        impl serde_json::ser::Formatter for Interior {
            fn begin_string<W: ?Sized + Write>(&mut self, _: &mut W) -> std::io::Result<()> {
                Ok(())
            }
            fn end_string<W: ?Sized + Write>(&mut self, _: &mut W) -> std::io::Result<()> {
                Ok(())
            }
        }
        let len = text.len();
        text.serialize(&mut serde_json::Serializer::with_formatter(&mut *self.0, Interior))?;
        self.1 = self
            .1
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("text length overflow"))?;
        Ok(())
    }
    /// Append a string view without decoding and re-encoding its already valid interior.
    pub(crate) fn push_string(&mut self, view: JsonView<'_>) -> Result<bool> {
        if !view.is_string()? {
            return Ok(false);
        }
        struct Interior<'a> {
            writer: &'a mut dyn Write,
            first: bool,
            last: Option<u8>,
            count: StringCount,
        }
        impl Write for Interior<'_> {
            fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
                let length = bytes.len();
                if bytes.is_empty() {
                    return Ok(0);
                }
                if self.first {
                    if bytes[0] != b'"' {
                        return Err(std::io::Error::other("missing string quote"));
                    }
                    self.first = false;
                    bytes = &bytes[1..];
                }
                if !bytes.is_empty() {
                    if let Some(last) = self.last.take() {
                        self.writer.write_all(&[last])?;
                        self.count.feed(&[last])?;
                    }
                    self.writer.write_all(&bytes[..bytes.len() - 1])?;
                    self.count.feed(&bytes[..bytes.len() - 1])?;
                    self.last = bytes.last().copied();
                }
                Ok(length)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.writer.flush()
            }
        }
        let mut interior = Interior {
            writer: self.0,
            first: true,
            last: None,
            count: StringCount::default(),
        };
        view.write_canonical(&mut interior)?;
        anyhow::ensure!(interior.last == Some(b'"'), "missing closing string quote");
        let len = interior.count.finish()?;
        self.1 = self
            .1
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("text length overflow"))?;
        Ok(true)
    }
    /// Append a value's canonical JSON document as decoded text.
    pub(crate) fn push_json(&mut self, view: JsonView<'_>) -> Result<()> {
        let len = write_quoted_json(view, self.0)?;
        self.1 = self
            .1
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("text length overflow"))?;
        Ok(())
    }
}

pub(crate) fn append(
    source: &JsonSource,
    arena: &Arc<ByteSource>,
    produce: impl FnOnce(&mut Builder<'_>) -> Result<()>,
) -> Result<Text> {
    let mut decoded_len = 0;
    let (offset, len) = arena.append(|writer| {
        decoded_len = generate_string_with_len(writer, produce)?;
        Ok(())
    })?;
    source.generated_string_text(arena.clone(), offset, len, decoded_len)
}

// Only a successful complete quoted string may reach generated_string_text; writer and producer
// failures leave unpublished append bytes. Builder fields prevent callers from emitting raw bytes.
pub(crate) fn generate_string(
    writer: &mut dyn Write,
    produce: impl FnOnce(&mut Builder<'_>) -> Result<()>,
) -> Result<()> {
    generate_string_with_len(writer, produce).map(|_| ())
}

fn generate_string_with_len(
    writer: &mut dyn Write,
    produce: impl FnOnce(&mut Builder<'_>) -> Result<()>,
) -> Result<usize> {
    writer.write_all(b"\"")?;
    let mut builder = Builder(writer, 0);
    produce(&mut builder)?;
    let len = builder.1;
    writer.write_all(b"\"")?;
    Ok(len)
}

fn write_quoted_json(view: crate::json::JsonView<'_>, writer: &mut dyn std::io::Write) -> Result<usize> {
    use std::io::Write;
    struct Escaped<'a> {
        writer: &'a mut dyn Write,
        scratch: Vec<u8>,
        len: usize,
    }
    impl Write for Escaped<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.len = self
                .len
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("text length overflow"))?;
            for chunk in bytes.chunks(64 * 1024) {
                self.scratch.clear();
                self.scratch.reserve(chunk.len().saturating_mul(2));
                for &byte in chunk {
                    if matches!(byte, b'"' | b'\\') {
                        self.scratch.push(b'\\');
                    }
                    self.scratch.push(byte);
                }
                self.writer.write_all(&self.scratch)?;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }
    let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, writer);
    let mut escaped = Escaped {
        writer: &mut buffered,
        scratch: Vec::new(),
        len: 0,
    };
    view.write_canonical(&mut escaped)?;
    let len = escaped.len;
    buffered.flush()?;
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lazy::Resolver;

    #[test]
    fn decoded_push_matches_canonical_serializer_bytes_and_lengths() {
        let controls = (0..32).map(char::from).collect::<String>();
        for body in [
            String::new(),
            "plain payload ".repeat(20_000),
            format!("🌙λ\n\"\\{controls}\u{7f}").repeat(8192),
            format!("{}🌙{}", "x".repeat(65535), "界".repeat(22000)),
        ] {
            let mut bytes = Vec::new();
            let len = generate_string_with_len(&mut bytes, |builder| {
                builder.push("prefix\n")?;
                builder.push(&body)?;
                builder.push("\"suffix\\")
            })
            .unwrap();
            let expected = format!("prefix\n{body}\"suffix\\");
            assert_eq!(bytes, serde_json::to_vec(&expected).unwrap());
            assert_eq!(len, expected.len());
        }
    }

    #[test]
    fn counted_generated_lengths_match_decoding_and_validate_every_demand() {
        let world = crate::test_world("generated-string-length");
        let path = world.base.join("original");
        let journal = world.base.join("original-journal");
        std::fs::write(&path, "original snapshot").unwrap();
        let mut reads = crate::source::ReadSet::default();
        reads.record(&journal);
        let source = JsonSource::capture_with_reads(&path, &reads).unwrap();
        let arena = Arc::new(ByteSource::arena().unwrap());
        let body = format!("a🌙{}\n\"\\", (0..32).map(char::from).collect::<String>()).repeat(8192);
        let value = serde_json::json!({"body":body,"other":[false,null,42]});
        let text = append(&source, &arena, |writer| {
            writer.push("prefix\n")?;
            assert!(writer.push_string(JsonView::from(value.get("body").unwrap()))?);
            writer.push_json(JsonView::from(&value))
        })
        .unwrap();
        let expected = format!("prefix\n{body}{}", serde_json::to_string(&value).unwrap());
        let Text::Source(mut counted) = text else {
            panic!("source carrier required")
        };
        assert_eq!(counted.known_len, Some(expected.len()));
        for _ in 0..3 {
            assert_eq!(counted.try_decoded_len().unwrap(), expected.len());
        }
        assert_eq!(counted.try_resolve().unwrap(), expected);
        // Nonzero prefixes keep the strict boundary checker rather than trusting manually set skip.
        counted.skip = "prefix\na".len() + 1;
        assert!(counted.try_decoded_len().is_err());
        counted.skip = 0;
        std::fs::write(&journal, "created").unwrap();
        assert!(
            counted.try_decoded_len().is_err(),
            "known length must validate auxiliary absence"
        );
        std::fs::remove_file(&journal).unwrap();
        arena.file().unwrap().set_len(0).unwrap();
        assert!(
            counted.try_decoded_len().is_err(),
            "known length must check backing bounds"
        );
    }

    #[test]
    fn canonical_string_length_counter_handles_fragmented_escapes() {
        let text = format!("🌙\n\"\\{}", (0..32).map(char::from).collect::<String>());
        let encoded = serde_json::to_vec(&text).unwrap();
        for chunk in [1, 2, 3, 7, 65536] {
            let mut counter = StringCount::default();
            for bytes in encoded[1..encoded.len() - 1].chunks(chunk) {
                counter.feed(bytes).unwrap();
            }
            assert_eq!(counter.finish().unwrap(), text.len());
        }
        // The counter is robust to strict noncanonical encodings too, including split pairs.
        let mut counter = StringCount::default();
        for byte in br"\uD83C\uDF19\u03bb\u007f" {
            counter.feed(&[*byte]).unwrap();
        }
        assert_eq!(counter.finish().unwrap(), "🌙λ\u{7f}".len());
    }

    #[test]
    fn generated_string_matches_full_codec_and_keeps_original_provenance() {
        let world = crate::test_world("generated-string-source");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot provenance").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let arena = Arc::new(ByteSource::arena().unwrap());
        let body = "🌙\n\"\\\t".repeat(8192);
        let input = serde_json::json!({"body":body, "scalar":42});
        let wire = serde_json::to_vec(&input).unwrap();
        let (offset, len) = arena
            .append(|out| {
                out.write_all(&wire)?;
                Ok(())
            })
            .unwrap();
        let text = source
            .read_backing_value(arena.clone(), offset, len, |view| {
                append(&source, &arena, |builder| {
                    builder.push("prefix\n")?;
                    assert!(builder.push_string(view.get("body")?.unwrap())?);
                    assert!(!builder.push_string(view.get("scalar")?.unwrap())?);
                    builder.push("\n")?;
                    builder.push_json(view)
                })
            })
            .unwrap();
        assert!(matches!(text, Text::Source(_)));
        let expected = format!("prefix\n{body}\n{}", serde_json::to_string(&input).unwrap());
        assert_eq!(text.try_resolve(&Resolver::new(None)).unwrap(), expected);
        assert_eq!(text.try_decoded_len(&Resolver::new(None)).unwrap(), expected.len());
        let small = append(&source, &arena, |builder| builder.push("small🌙")).unwrap();
        assert_eq!(small.inline_str(), Some("small🌙"));
        std::fs::write(&path, "changed snapshot").unwrap();
        assert!(text.try_resolve_prefix(&Resolver::new(None), 8).is_err());
        assert!(text.try_decoded_len(&Resolver::new(None)).is_err());
        assert!(append(&source, &arena, |builder| builder.push("fresh")).is_err());
    }

    #[test]
    fn malformed_syntax_only_strings_and_failed_writers_never_publish_a_carrier() {
        let world = crate::test_world("generated-string-errors");
        let path = world.base.join("original");
        std::fs::write(&path, "snapshot provenance").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let arena = Arc::new(ByteSource::arena().unwrap());
        let accepted = append(&source, &arena, |builder| builder.push(&"keep🌙".repeat(8192))).unwrap();
        for suffix in ["\\uD800", "\\uDC00"] {
            let wire = format!("\"{}{}\"", "prefix".repeat(8192), suffix);
            let (offset, len) = arena
                .append(|out| {
                    out.write_all(wire.as_bytes())?;
                    Ok(())
                })
                .unwrap();
            for string_only in [true, false] {
                let mut visited = false;
                let result = source.read_backing_syntax(arena.clone(), offset, len, |view| {
                    visited = true;
                    append(&source, &arena, |builder| {
                        if string_only {
                            builder.push_string(view)?;
                            Ok(())
                        } else {
                            builder.push_json(view)
                        }
                    })
                });
                assert!(
                    visited,
                    "syntax-only view must exercise canonical strict string checking"
                );
                assert!(result.is_err(), "{suffix} string_only={string_only}");
            }
        }
        assert_eq!(
            accepted.try_resolve(&Resolver::new(None)).unwrap(),
            "keep🌙".repeat(8192)
        );
        assert!(append(&source, &arena, |builder| {
            builder.push("unpublished")?;
            anyhow::bail!("producer failure")
        })
        .is_err());
        struct Fails {
            remaining: usize,
        }
        impl Write for Fails {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("writer failure"));
                }
                let len = bytes.len().min(self.remaining);
                self.remaining -= len;
                Ok(len)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for body in [
            "unpublished".to_owned(),
            "long borrowed body".repeat(8192),
            "escaped\n\"\\🌙".repeat(8192),
        ] {
            let error = generate_string(&mut Fails { remaining: 8 }, |builder| builder.push(&body)).unwrap_err();
            assert!(error.to_string().contains("writer failure"));
        }
        assert_eq!(
            accepted.try_decoded_len(&Resolver::new(None)).unwrap(),
            "keep🌙".len() * 8192
        );
    }
}
