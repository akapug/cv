//! The one zstd decoder for compressed sources (Codex rollouts, Zed and OpenClaw blobs). Native
//! builds use libzstd, already linked into the shipped binary through tantivy and about 4x
//! ruzstd's decode rate (`show` of a 412 MB rollout: 28 G -> 14 G instructions); wasm keeps ruzstd.
//! Both decode exactly one frame, streaming, with a bounded window.
use std::io;
use std::io::Read;

/// A reader over the first zstd frame of `input`. A malformed header fails here, as ruzstd's
/// constructor did, so callers keep telling "not zstd" apart from a failure mid-stream.
#[cfg(all(feature = "zstd", not(target_family = "wasm")))]
pub(crate) fn decoder<R: Read>(input: R) -> io::Result<impl Read> {
    use std::io::BufRead;
    let mut decoded = io::BufReader::with_capacity(64 * 1024, zstd::stream::read::Decoder::new(input)?.single_frame());
    decoded.fill_buf()?;
    Ok(decoded)
}

#[cfg(all(feature = "zstd", target_family = "wasm"))]
pub(crate) fn decoder<R: Read>(input: R) -> io::Result<impl Read> {
    ruzstd::decoding::StreamingDecoder::new(input).map_err(io::Error::other)
}

/// A disabled decoder is an explicit failed demand, never compressed bytes presented as JSONL.
#[cfg(not(feature = "zstd"))]
pub(crate) fn decoder<R: Read>(_: R) -> io::Result<std::io::Empty> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "compressed sources require the `zstd` feature",
    ))
}

/// The decoded size the first frame header of `path` declares (RFC 8878 §3.1.1.1), if any.
/// An unverified size hint only; callers must capture/check the source before trusting its data.
pub(crate) fn content_size(path: &std::path::Path) -> Option<u64> {
    let mut head = [0u8; 18];
    let mut file = std::fs::File::open(path).ok()?;
    let mut len = 0;
    while len < head.len() {
        match file.read(&mut head[len..]).ok()? {
            0 => break,
            n => len += n,
        }
    }
    frame_content_size(&head[..len])
}

fn frame_content_size(head: &[u8]) -> Option<u64> {
    if head.get(..4)? != 0xFD2F_B528u32.to_le_bytes() {
        return None;
    }
    let descriptor = *head.get(4)?;
    let single_segment = descriptor & 0x20 != 0;
    let at = 5 + usize::from(!single_segment) + [0, 1, 2, 4][usize::from(descriptor & 3)];
    let size = match descriptor >> 6 {
        0 if single_segment => 1,
        0 => return None,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut value = [0u8; 8];
    value[..size].copy_from_slice(head.get(at..at + size)?);
    Some(u64::from_le_bytes(value) + if size == 2 { 256 } else { 0 })
}

#[cfg(all(test, feature = "zstd", not(target_family = "wasm")))]
mod tests {
    /// Every header layout the encoder writes declares the size it was given; a stream of unknown
    /// size and a non-zstd file declare none.
    #[test]
    fn frame_headers_declare_their_content_size() {
        for len in [0usize, 1, 255, 256, 300, 65_791, 65_792, 1 << 20] {
            let text = vec![b'x'; len];
            assert_eq!(
                super::frame_content_size(&zstd::bulk::compress(&text, 1).unwrap()),
                Some(len as u64)
            );
        }
        let mut streamed = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        std::io::Write::write_all(&mut streamed, b"unknown size").unwrap();
        assert_eq!(super::frame_content_size(&streamed.finish().unwrap()), None);
        assert_eq!(super::frame_content_size(b"{\"type\":\"session_meta\"}"), None);
    }
}
