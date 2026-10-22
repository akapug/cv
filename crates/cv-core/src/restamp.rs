//! The one session-id restamp for Claude JSONL records, as byte surgery.
//!
//! Every record-level reshape of a Claude transcript (`prune`, `rewind`, `adopt`) writes a NEW
//! session: the same lines with a new `sessionId` (and `session_id`, the other spelling Claude Code
//! writes on most records since ~2.1.25x). This module replaces exactly those two top-level string
//! values, located by `RawValue` offsets, and leaves every other byte of the line as it was — key
//! order, whitespace, number spellings, non-ASCII text. A `sessionId` nested inside content (a tool
//! output quoting a transcript) is content and is not touched. The property, pinned below:
//! `restamp(restamp(x, a), b) == x` when `b` is the id `x` carried.

use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct TopIds<'a> {
    #[serde(borrow, rename = "sessionId")]
    session_id: Option<&'a RawValue>,
    #[serde(borrow, rename = "session_id")]
    session_id_snake: Option<&'a RawValue>,
}

/// Raw transform output ids also become filenames/resource directory names. Keep them within the
/// selected output directory; this does not restrict ids stored in arbitrary IR or restamped text.
pub(crate) fn validate_output_id(id: &str) -> anyhow::Result<()> {
    let mut components = std::path::Path::new(id).components();
    anyhow::ensure!(
        !id.is_empty()
            && !id.contains(['/', '\\', '\0'])
            && matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none(),
        "new session id must be a nonempty filename component, not a path: {id:?}"
    );
    Ok(())
}

/// Restamp one transcript line: replace the top-level `sessionId` / `session_id` string values with
/// `new_id`, leaving every other byte as it was. `None` when the line carries no top-level session
/// id (non-JSON, or a record without one): the caller keeps it verbatim.
pub fn restamp_line(line: &str, new_id: &str) -> Option<String> {
    let ids: TopIds = serde_json::from_str(line).ok()?;
    let mut spans: Vec<(usize, usize)> = [ids.session_id, ids.session_id_snake]
        .into_iter()
        .flatten()
        .map(RawValue::get)
        .filter(|raw| raw.starts_with('"'))
        .map(|raw| {
            let start = raw.as_ptr() as usize - line.as_ptr() as usize;
            (start, start + raw.len())
        })
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort();
    let quoted = serde_json::to_string(new_id).expect("a string serializes");
    let mut out = String::with_capacity(line.len() + 8);
    let mut at = 0;
    for (s, e) in spans {
        out.push_str(&line[at..s]);
        out.push_str(&quoted);
        at = e;
    }
    out.push_str(&line[at..]);
    Some(out)
}

/// Selected during strict producer validation; preserve derived-struct id acceptance.
pub(crate) fn source_id_spans(
    view: crate::json::JsonView<'_>,
    keys: &[&'static str],
) -> anyhow::Result<Vec<(u64, u64)>> {
    use crate::harness::store::document::{Fields, InvalidStruct};
    let ids = match Fields::parse_struct(view, keys) {
        Ok(Some(ids)) => ids,
        Ok(None) => return Ok(Vec::new()),
        Err(e) if e.is::<InvalidStruct>() => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut spans = Vec::new();
    for key in keys {
        if let Some(value) = ids.get(key) {
            if value.is_string()? {
                let (offset, len) = value
                    .source_range()?
                    .ok_or_else(|| anyhow::anyhow!("expected source id range"))?;
                spans.push((offset, offset + len));
            }
        }
    }
    Ok(spans)
}

/// Copy captured, already selected id spans without repeating source JSON scans.
pub(crate) fn write_restamped_spans(
    source: &crate::json::JsonSource,
    file: &std::fs::File,
    offset: u64,
    len: u64,
    new_id: &str,
    spans: &mut [(u64, u64)],
    output: &mut impl std::io::Write,
) -> anyhow::Result<bool> {
    use crate::harness::store::jsonl::ranges::{self, Range};
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    spans.sort_unstable();
    let stamped = !spans.is_empty();
    let mut at = offset;
    for &(start, end) in spans.iter() {
        std::io::copy(
            &mut ranges::reader(
                file,
                Range {
                    offset: at,
                    len: start - at,
                },
            )?,
            output,
        )?;
        serde_json::to_writer(&mut *output, new_id)?;
        at = end;
    }
    std::io::copy(
        &mut ranges::reader(
            file,
            Range {
                offset: at,
                len: offset + len - at,
            },
        )?,
        output,
    )?;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    Ok(stamped)
}

#[cfg(test)]
use crate::derive::{transcript as write_restamped_transcript, Counts as RestampCounts};

/// A restamped transcript and what the restamp touched.
pub struct Restamped {
    pub body: String,
    /// Non-blank lines seen.
    pub lines: usize,
    /// Lines that carried a top-level id and were restamped.
    pub stamped: usize,
}

/// Restamp a whole transcript. Line endings and blank lines are preserved as they were.
pub fn restamp(text: &str, new_id: &str) -> Restamped {
    let mut body = String::with_capacity(text.len() + 64);
    let (mut lines, mut stamped) = (0, 0);
    for piece in text.split_inclusive('\n') {
        let (line, end) = match piece.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (piece, ""),
        };
        lines += usize::from(!crate::harness::store::json::trim_str(line).is_empty());
        match restamp_line(line, new_id) {
            Some(new) => {
                stamped += 1;
                body.push_str(&new);
            }
            None => body.push_str(line),
        }
        body.push_str(end);
    }
    Restamped { body, lines, stamped }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stream a captured UTF-8 record with the same top-level id surgery as `restamp_line`.
    /// The producer chooses UTF-8/JSON acceptance and owns staging of partial writes. Malformed JSON
    /// and duplicate known id fields are copied verbatim, matching the borrowed serde struct above.
    fn write_restamped_source(
        source: &crate::json::JsonSource,
        file: &std::fs::File,
        offset: u64,
        len: u64,
        new_id: &str,
        output: &mut impl std::io::Write,
    ) -> anyhow::Result<bool> {
        use crate::harness::store::document::InvalidStruct;
        source.check_metadata(file.metadata()?)?;
        source.check()?;
        let mut spans = if len == 0 {
            Vec::new()
        } else {
            let keys = crate::harness::claude::IDENTITY.session_keys;
            match source.read_range_syntax(offset, len, |view| source_id_spans(view, keys)) {
                Ok(spans) => spans,
                Err(e) if e.is::<crate::json::InvalidJson>() || e.is::<InvalidStruct>() => Vec::new(),
                Err(e) => return Err(e),
            }
        };
        write_restamped_spans(source, file, offset, len, new_id, &mut spans, output)
    }

    #[test]
    fn transcript_writer_matches_old_restamp_bytes_and_counts_for_mixed_rows() {
        let world = cv_testkit::World::new("streaming-restamp-transcript");
        let path = world.base.join("rows.jsonl");
        let small = concat!(
            "\r\n  \n",
            " { \"sessionId\" : \"dead\", \"n\":1.50, \"nested\":{\"sessionId\":\"keep\"} }\r\n",
            "{\"sessionId\":\"dead\",\"sessionId\":\"duplicate\"}\n",
            "[\"dead\",\"snake\"]\n",
            "{\"session_id\":12}\n{\"sessionId\":\"dead\",\"ignored\":1e9999}"
        );
        let large = format!(
            "{small}\n{{\"sessionId\":\"dead\",\"text\":\"{}\",\"session_id\":\"dead\"}}\r\n{}\n{{\"text\":\"{}\"}}\n{{\"sessionId\":\"dead\"}}",
            "λ\\n".repeat(30000), " ".repeat(100000), "x".repeat(100000)
        );
        for original in ["", small, large.as_str()] {
            std::fs::write(&path, original).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let expected = restamp(original, "new\"id\\λ");
            let mut actual = Vec::new();
            let counts = write_restamped_transcript(
                &source,
                &file,
                "new\"id\\λ",
                crate::harness::claude::IDENTITY,
                &mut actual,
            )
            .unwrap();
            assert_eq!(actual, expected.body.as_bytes());
            assert_eq!(
                counts,
                RestampCounts {
                    lines: expected.lines,
                    stamped: expected.stamped,
                    bytes: expected.body.len()
                }
            );
        }
        // A row that is not one JSON value, short or past the in-memory limit, is refused by
        // byte rather than copied; the text restamp still copies it, so the two now differ.
        for malformed in ["not json ☃", &format!("{} trailing malformed ☃", "x".repeat(100000))] {
            let original = format!("{small}\n{malformed}\n{{\"sessionId\":\"dead\"}}");
            std::fs::write(&path, &original).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let mut actual = Vec::new();
            let error =
                write_restamped_transcript(&source, &file, "new", crate::harness::claude::IDENTITY, &mut actual)
                    .unwrap_err();
            assert!(error.is::<crate::json::InvalidJson>(), "{error:#}");
            assert!(
                error.to_string().contains(&format!("byte {}", small.len() + 1)),
                "{error:#}"
            );
        }
    }

    #[test]
    fn source_writer_matches_borrowed_struct_bytes_and_rejections() {
        let world = cv_testkit::World::new("source-restamp");
        let path = world.base.join("rows.jsonl");
        for line in [
            "",
            "not json",
            "null",
            "{}",
            r#"{"sessionId":"x","session_id":"x","nested":{"sessionId":"keep"}}"#,
            r#" { "sessionId" : "x", "n":1.50, "session_id":true } "#,
            r#"{"sessionId":"x","sessionId":"duplicate"}"#,
            r#"{"sessionId":null,"session_id":12}"#,
            r#"["x","y"]"#,
            r#"["x","y","extra"]"#,
            r#"{"ignored":1e9999,"sessionId":"x"}"#,
        ] {
            std::fs::write(&path, line).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let mut output = Vec::new();
            let stamped = write_restamped_source(&source, &file, 0, line.len() as u64, "new\"☃", &mut output).unwrap();
            let expected = restamp_line(line, "new\"☃");
            assert_eq!(stamped, expected.is_some(), "{line}");
            assert_eq!(
                String::from_utf8(output).unwrap(),
                expected.unwrap_or_else(|| line.to_owned()),
                "{line}"
            );
        }
    }

    #[test]
    fn restamp_line_ignores_lines_without_a_top_level_id() {
        assert_eq!(restamp_line("not json", "y"), None);
        assert_eq!(restamp_line(r#"{"a":{"sessionId":"x"}}"#, "y"), None);
        assert_eq!(restamp_line(r#"{"sessionId":null}"#, "y"), None);
        assert_eq!(
            restamp_line(r#"{"sessionId":"x","session_id":"x","k":"x"}"#, "y").unwrap(),
            r#"{"sessionId":"y","session_id":"y","k":"x"}"#
        );
    }

    /// Only the two top-level ids move; spacing, number spellings, nested ids and non-ASCII text
    /// are the source's bytes, so restamping back gives the source.
    #[test]
    fn restamp_is_byte_surgery_and_undoes_itself() {
        let src = concat!(
            "{\"parentUuid\":null,\"type\":\"user\",\"message\":{\"content\":\"brief — ☃\"},\"uuid\":\"u1\",\"sessionId\":\"dead\"}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"{\\\"sessionId\\\":\\\"dead\\\"}\"}]},\"uuid\":\"u2\",\"sessionId\": \"dead\" , \"n\":1.50, \"session_id\":\"dead\"}\n",
            "\n",
            "{\"type\":\"summary\",\"summary\":\"no session id here\"}"
        );
        let r = restamp(src, "live");
        assert_eq!((r.lines, r.stamped), (3, 2));
        assert_eq!(r.body.matches("live").count(), 3, "two ids on line 2, one on line 1");
        assert!(
            r.body.contains("{\\\"sessionId\\\":\\\"dead\\\"}"),
            "nested id is content"
        );
        assert!(r
            .body
            .contains("\"sessionId\": \"live\" , \"n\":1.50, \"session_id\":\"live\"}"));
        assert!(r.body.contains("brief — ☃"));
        assert!(r.body.ends_with("\"no session id here\"}"));
        assert_eq!(restamp(&r.body, "dead").body, src);
    }
}
