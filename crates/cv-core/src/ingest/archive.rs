//! Shared, fallible in-memory extraction for desktop and browser harness uploads.
use anyhow::{bail, Context, Result};
use std::io::{Read, Seek};

/// Bound actual decoded content, including archives whose size headers are forged.
#[derive(Clone, Copy)]
pub struct Limits {
    pub entries: usize,
    pub entry_bytes: u64,
    pub total_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 10_000,
            entry_bytes: 256 * 1024 * 1024,
            total_bytes: 512 * 1024 * 1024,
        }
    }
}

/// Extract complete file entries, preserving their original nested names for harness sniffing.
/// Partial extraction is never published: an oversized, corrupt or unsupported entry rejects
/// the upload instead of quietly omitting transcript history or required companion files.
pub fn extract(reader: impl Read + Seek, limits: Limits) -> Result<Vec<(String, Vec<u8>)>> {
    let mut archive = zip::ZipArchive::new(reader).context("not a readable zip")?;
    anyhow::ensure!(
        archive.len() <= limits.entries,
        "zip has {} entries, exceeding the extraction limit ({})",
        archive.len(),
        limits.entries
    );
    let mut files = Vec::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading zip entry {index}"))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_owned();
        let cap = limits.entry_bytes.min(limits.total_bytes.saturating_sub(total));
        if entry.size() > cap {
            bail!(
                "zip entry {name:?} declares {} bytes, exceeding the remaining extraction limit ({cap} bytes)",
                entry.size()
            );
        }
        let mut bytes = Vec::new();
        (&mut entry)
            .take(cap.saturating_add(1))
            .read_to_end(&mut bytes)
            .with_context(|| format!("decompressing zip entry {name:?}"))?;
        if bytes.len() as u64 > cap {
            bail!("zip entry {name:?} decoded past the extraction limit ({cap} bytes)");
        }
        total += bytes.len() as u64;
        files.push((name, bytes));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_directory("nested/", zip::write::SimpleFileOptions::default())
            .unwrap();
        for (name, bytes) in entries {
            writer
                .start_file(
                    *name,
                    zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn archive_entry_count_is_bounded_even_for_empty_files() {
        let bytes = archive(&[("a", b""), ("b", b"")]);
        let result = extract(
            Cursor::new(bytes),
            Limits {
                entries: 2,
                ..Limits::default()
            },
        );
        assert!(result.unwrap_err().to_string().contains("entries"));
    }
    #[test]
    fn nested_entries_and_exact_limits_are_preserved() {
        let bytes = archive(&[("nested/session.jsonl", b"1234"), ("nested/meta.json", b"12")]);
        let extracted = extract(
            Cursor::new(bytes),
            Limits {
                entries: 10,
                entry_bytes: 4,
                total_bytes: 6,
            },
        )
        .unwrap();
        assert_eq!(
            extracted,
            vec![
                ("nested/session.jsonl".into(), b"1234".to_vec()),
                ("nested/meta.json".into(), b"12".to_vec())
            ]
        );
    }
    #[test]
    fn oversized_entry_or_total_rejects_the_complete_upload() {
        let bytes = archive(&[("first", b"1234"), ("second", b"123")]);
        assert!(extract(
            Cursor::new(&bytes),
            Limits {
                entries: 10,
                entry_bytes: 3,
                total_bytes: 20
            }
        )
        .unwrap_err()
        .to_string()
        .contains("first"));
        assert!(extract(
            Cursor::new(&bytes),
            Limits {
                entries: 10,
                entry_bytes: 4,
                total_bytes: 6
            }
        )
        .unwrap_err()
        .to_string()
        .contains("second"));
    }
    #[test]
    fn forged_uncompressed_size_never_bypasses_decoded_limit() {
        let mut bytes = archive(&[("payload", &[b'x'; 100])]);
        // Both directory/local file headers underreport content. Reading must still either
        // reject the corrupt entry or discover the decoded cap violation.
        for index in 0..bytes.len().saturating_sub(30) {
            let offset = if bytes[index..].starts_with(b"PK\x01\x02") {
                Some(24)
            } else if bytes[index..].starts_with(b"PK\x03\x04") {
                Some(22)
            } else {
                None
            };
            if let Some(offset) = offset {
                bytes[index + offset..index + offset + 4].fill(0);
            }
        }
        assert!(extract(
            Cursor::new(bytes),
            Limits {
                entries: 10,
                entry_bytes: 8,
                total_bytes: 16
            }
        )
        .is_err());
    }
}
