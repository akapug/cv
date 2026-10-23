//! Bounded UTF8-lossy transport strings, using serde's own JSON escaping.
use serde::Serialize;
use serde_json::ser::Formatter;
use std::io::{self, Read, Write};

/// Equivalent to `String::from_utf8_lossy` over the complete reader, including split scalars.
/// Callback chunks are temporary and bounded; failures stop reading immediately.
pub fn for_each_lossy_utf8_chunk(input: impl Read, f: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
    crate::utf8::read(input, crate::utf8::Verdict::Replace, f).map(|_| ())
}

/// Write one quoted JSON string without retaining its complete decoded or escaped bytes.
pub fn write_json_string(out: &mut impl Write, input: impl Read) -> io::Result<()> {
    // Each chunk serializes as a string. Suppress its quotes so the escaped fragments
    // compose inside the single pair written around the complete stream.
    struct Fragment;
    impl Formatter for Fragment {
        fn begin_string<W: ?Sized + Write>(&mut self, _: &mut W) -> io::Result<()> {
            Ok(())
        }
        fn end_string<W: ?Sized + Write>(&mut self, _: &mut W) -> io::Result<()> {
            Ok(())
        }
    }
    out.write_all(b"\"")?;
    for_each_lossy_utf8_chunk(input, |text| {
        text.serialize(&mut serde_json::Serializer::with_formatter(&mut *out, Fragment))
            .map_err(io::Error::other)
    })?;
    out.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Short<'a> {
        bytes: &'a [u8],
        chunk: usize,
    }
    impl Read for Short<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(self.chunk).min(self.bytes.len());
            out[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }
    #[test]
    fn lossy_transport_matches_whole_string_across_boundaries() {
        let mut input = "λ🌙\"\\\n\t/\0".repeat(8000).into_bytes();
        input.extend_from_slice(&[0xff, 0xe2, 0x82, b'x', 0xf0, 0x9f, 0x92]);
        let expected = serde_json::to_vec(&String::from_utf8_lossy(&input)).unwrap();
        for chunk in [1, 2, 3, 5, 65535, 65536] {
            let mut actual = Vec::new();
            write_json_string(&mut actual, Short { bytes: &input, chunk }).unwrap();
            assert_eq!(actual, expected, "chunk {chunk}");
        }
    }
}
