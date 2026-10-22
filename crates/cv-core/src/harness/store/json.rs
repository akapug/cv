//! Allocation-free checks that serde's ignored/raw values omit. Call only after JSON syntax
//! validation; these checks preserve Value's finite numbers, UTF-8/UTF-16 and depth acceptance.

/// JSON's insignificant whitespace (RFC 8259): space, tab, LF and CR, never form feed or a Unicode
/// space. Every reader of a JSONL line trims by exactly this, so a line is blank to all or to none.
pub(crate) fn is_whitespace(byte: &u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// `bytes` without the JSON whitespace around it.
pub(crate) fn trim(bytes: &[u8]) -> &[u8] {
    let lead = bytes.iter().take_while(|b| is_whitespace(b)).count();
    let tail = bytes[lead..].iter().rev().take_while(|b| is_whitespace(b)).count();
    &bytes[lead..bytes.len() - tail]
}

/// [`trim`] over text.
pub(crate) fn trim_str(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r'])
}

/// The bytes a validation pass reads: a slice in memory, or a positioned source read in windows
/// (`document::range::Windowed`). Indexes are relative to the pass's first byte.
pub(crate) trait ValidationBytes {
    fn get(&mut self, i: usize) -> Option<u8>;
    /// `len` bytes from `i`; None past the end (never at a window boundary).
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]>;
    /// Whether `i..end` is UTF-8 (a pass over `str` bytes knows it is).
    fn utf8(&mut self, i: usize, end: usize) -> bool;
    /// Whether the number token at `i..end` is accepted: as a finite `serde_json::Number`
    /// (`strict`), or by JSON grammar alone.
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool;
}
impl ValidationBytes for &[u8] {
    #[inline]
    fn get(&mut self, i: usize) -> Option<u8> {
        (**self).get(i).copied()
    }
    #[inline]
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]> {
        (**self).get(i..i.checked_add(len)?)
    }
    fn utf8(&mut self, i: usize, end: usize) -> bool {
        std::str::from_utf8(&self[i..end]).is_ok()
    }
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool {
        number_accepted(&self[i..end], strict)
    }
}
/// `str` bytes: UTF-8 is known.
pub(crate) struct Text<'a>(pub &'a [u8]);
impl ValidationBytes for Text<'_> {
    #[inline]
    fn get(&mut self, i: usize) -> Option<u8> {
        self.0.get(i).copied()
    }
    #[inline]
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]> {
        self.0.get(i..i.checked_add(len)?)
    }
    fn utf8(&mut self, _: usize, _: usize) -> bool {
        true
    }
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool {
        number_accepted(&self.0[i..end], strict)
    }
}
pub(crate) fn number_accepted(token: &[u8], strict: bool) -> bool {
    if strict {
        serde_json::from_slice::<serde_json::Number>(token).is_ok()
    } else {
        serde_json::from_slice::<&serde_json::value::RawValue>(token)
            .is_ok_and(|raw| matches!(raw.get().as_bytes().first(), Some(b'-' | b'0'..=b'9')))
    }
}

/// One pass over a JSON value from any byte source, accepting exactly what
/// `serde_json::from_str::<Value>` accepts when `strict` (JSON grammar, strings without control
/// characters or invalid escapes including lone surrogates, finite numbers, Value's nesting
/// limit, nothing after the value), and what `RawValue` accepts otherwise (lone UTF-16 escapes
/// and out-of-range number tokens pass; grammar, control characters and UTF-8 still apply,
/// without Value's nesting limit).
/// `depth` levels are already open around the value. `span` sees the bounds of every string
/// and container, so a caller can index them for later structural selection.
/// Returns the value's bounds, or None when it is not accepted.
pub(crate) fn validate_value(
    b: &mut impl ValidationBytes,
    strict: bool,
    depth: usize,
    mut span: impl FnMut(u8, usize, usize),
) -> Option<(usize, usize)> {
    // Open containers: (start, is_object). Value rejects the 128th level; grammar alone has no
    // limit, so deeper levels spill to the heap only when a document actually nests that far.
    let mut open = [(0usize, false); 128];
    let mut deep: Vec<(usize, bool)> = Vec::new();
    let base = depth;
    let mut depth = 0;
    let mut i = skip_ws(b, 0);
    let first = i;
    loop {
        // A value starts here.
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b'{' | b'[') => {
                let object = b.get(i) == Some(b'{');
                match open.get_mut(depth) {
                    Some(slot) => *slot = (i, object),
                    None => deep.push((i, object)),
                }
                depth += 1;
                if strict && base + depth >= 128 {
                    return None;
                }
                i = skip_ws(b, i + 1);
                if b.get(i) == Some(if object { b'}' } else { b']' }) {
                    depth -= 1;
                    let start = match open.get(depth) {
                        Some(open) => open.0,
                        None => deep.pop()?.0,
                    };
                    span(if object { b'{' } else { b'[' }, start, i + 1);
                    i += 1;
                } else {
                    if object {
                        i = key(b, i, strict, &mut span)?;
                    }
                    continue;
                }
            }
            Some(b'"') => {
                let end = string(b, i, strict)?;
                span(b'"', i, end);
                i = end;
            }
            Some(b't') if b.slice(i, 4) == Some(b"true") => i += 4,
            Some(b'f') if b.slice(i, 5) == Some(b"false") => i += 5,
            Some(b'n') if b.slice(i, 4) == Some(b"null") => i += 4,
            Some(b'-' | b'0'..=b'9') => {
                let start = i;
                while b.get(i).is_some_and(|c| !is_whitespace(&c) && !b",]}".contains(&c)) {
                    i += 1;
                }
                if !b.number(start, i, strict) {
                    return None;
                }
            }
            _ => return None,
        }
        // A value ended: close containers until another value is due.
        loop {
            let end = i;
            i = skip_ws(b, i);
            if depth == 0 {
                return b.get(i).is_none().then_some((first, end));
            }
            let (start, object) = *open.get(depth - 1).unwrap_or_else(|| &deep[depth - 1 - open.len()]);
            match b.get(i) {
                Some(b',') => {
                    i = skip_ws(b, i + 1);
                    i = key_or_value(b, i, object, strict, &mut span)?;
                    break;
                }
                Some(b'}') if object => {}
                Some(b']') if !object => {}
                _ => return None,
            }
            depth -= 1;
            deep.truncate(depth.saturating_sub(open.len()));
            span(if object { b'{' } else { b'[' }, start, i + 1);
            i += 1;
        }
    }
}

/// After a comma: an object's next key and colon, or an array's next value start.
fn key_or_value(
    b: &mut impl ValidationBytes,
    i: usize,
    object: bool,
    strict: bool,
    span: &mut impl FnMut(u8, usize, usize),
) -> Option<usize> {
    if object {
        key(b, i, strict, span)
    } else {
        Some(i)
    }
}

fn skip_ws(b: &mut impl ValidationBytes, mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| is_whitespace(&c)) {
        i += 1;
    }
    i
}

/// An object key and its colon; the offset after the colon.
fn key(b: &mut impl ValidationBytes, i: usize, strict: bool, span: &mut impl FnMut(u8, usize, usize)) -> Option<usize> {
    if b.get(i) != Some(b'"') {
        return None;
    }
    let end = string(b, i, strict)?;
    span(b'"', i, end);
    let i = skip_ws(b, end);
    (b.get(i) == Some(b':')).then_some(i + 1)
}

/// The end of a valid JSON string starting at the quote `i`.
fn string(b: &mut impl ValidationBytes, i: usize, strict: bool) -> Option<usize> {
    let mut j = i + 1;
    loop {
        j = special(b, j);
        match b.get(j)? {
            b'"' => {
                return b.utf8(i + 1, j).then_some(j + 1);
            }
            b'\\' => match b.get(j + 1)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => j += 2,
                b'u' => {
                    let code = hex_quad(b.slice(j + 2, 4))?;
                    j += 6;
                    if !strict {
                        continue;
                    }
                    if (0xd800..=0xdbff).contains(&code) {
                        if b.slice(j, 2) != Some(b"\\u") {
                            return None;
                        }
                        let low = hex_quad(b.slice(j + 2, 4))?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return None;
                        }
                        j += 6;
                    } else if (0xdc00..=0xdfff).contains(&code) {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None, // A control character.
        }
    }
}

/// The first quote, backslash or control byte at or after `j`, eight bytes at a time.
fn special(b: &mut impl ValidationBytes, mut j: usize) -> usize {
    const ONES: u64 = u64::from_ne_bytes([1; 8]);
    const HIGH: u64 = u64::from_ne_bytes([0x80; 8]);
    let zero = |x: u64| x.wrapping_sub(ONES) & !x & HIGH;
    while let Some(chunk) = b.slice(j, 8) {
        let w = u64::from_le_bytes(chunk.try_into().unwrap());
        let found = zero(w ^ (ONES * b'"' as u64))
            | zero(w ^ (ONES * b'\\' as u64))
            | (w.wrapping_sub(ONES * 0x20) & !w & HIGH);
        if found != 0 {
            return j + (found.trailing_zeros() / 8) as usize;
        }
        j += 8;
    }
    while b.get(j).is_some_and(|c| c != b'"' && c != b'\\' && c >= 0x20) {
        j += 1;
    }
    j
}

fn hex_quad(b: Option<&[u8]>) -> Option<u16> {
    b?.iter()
        .try_fold(0u16, |value, c| Some(value * 16 + char::from(*c).to_digit(16)? as u16))
}

// The one JSON string-body codec: a body is the bytes between a string's quotes, read through
// any `BufRead` in bounded windows (a borrowed slice, a mapped span, a positioned range).

/// Decode a body through serde's string parser, one window at a time; at most one incomplete
/// escape or UTF-8 scalar carries into the next window. Stops early when `f` returns false (Ok
/// after a stop has not validated the unread suffix). Returns whether the body contained an
/// escape. Callbacks may run before a later malformed window or read error returns `Err`:
/// callers discard or reset what they accumulated on failure.
pub(crate) fn decode_body(reader: &mut impl std::io::BufRead, f: &mut impl FnMut(&str) -> bool) -> Result<bool, ()> {
    use serde::Deserializer;
    json_body_windows(reader, &mut |quoted| {
        let mut decoder = serde_json::Deserializer::from_slice(quoted);
        let keep_going = decoder.deserialize_str(JsonWindow(f)).map_err(|_| ())?;
        decoder.end().map_err(|_| ())?;
        Ok(keep_going)
    })
}

fn json_body_windows(
    reader: &mut impl std::io::BufRead,
    visit: &mut impl FnMut(&[u8]) -> Result<bool, ()>,
) -> Result<bool, ()> {
    // serde decodes an escaped window into its own scratch, so a decode holds two windows.
    const WINDOW: usize = 32 * 1024;
    // One buffer: an opening quote, then at most one window of body. Its capacity follows what
    // the reader offers, so a short body costs its length and a long one one window.
    let mut buf = vec![b'"'];
    let mut has_escape = false;
    loop {
        while buf.len() - 1 < WINDOW {
            let bytes = reader.fill_buf().map_err(|_| ())?;
            if bytes.is_empty() {
                break;
            }
            let n = (WINDOW + 1 - buf.len()).min(bytes.len());
            // n fills at most WINDOW body bytes after the opening quote; the extra byte
            // reserves its closing quote. Thus needed <= WINDOW + 2 and needed > buf.len().
            let needed = buf.len() + n + 1;
            if needed > buf.capacity() {
                buf.reserve_exact(needed.max(2 * buf.capacity()).min(WINDOW + 2) - buf.len());
            }
            buf.extend_from_slice(&bytes[..n]);
            reader.consume(n);
        }
        let raw = &buf[1..];
        let eof = raw.len() < WINDOW;
        if raw.is_empty() {
            return Ok(has_escape);
        }
        let mut end = if eof {
            raw.len()
        } else {
            complete_json_window_prefix(raw)
        };
        if !eof && raw[end - 1] >= 0x80 {
            let mut tail = end.saturating_sub(4);
            while tail > 0 && (raw[tail] & 0xc0) == 0x80 {
                tail -= 1;
            }
            if let Err(e) = std::str::from_utf8(&raw[tail..end]) {
                if e.error_len().is_none() {
                    end = tail + e.valid_up_to();
                }
            }
        }
        has_escape |= raw[..end].contains(&b'\\');
        // Close the quote in place over the first carried byte, then restore it.
        let close = end + 1;
        let carried = buf.get(close).copied();
        match carried {
            Some(_) => buf[close] = b'"',
            None => buf.push(b'"'),
        }
        let keep_going = visit(&buf[..close + 1]);
        match carried {
            Some(byte) => buf[close] = byte,
            None => {
                buf.pop();
            }
        }
        if !keep_going? {
            return Ok(has_escape);
        }
        buf.drain(1..close);
        if eof {
            return Ok(has_escape);
        }
    }
}

/// The callback borrows serde's decode scratch; neither pass needs an owned decoded String.
struct JsonWindow<'a, F>(&'a mut F);

impl<'de, F: FnMut(&str) -> bool> serde::de::Visitor<'de> for JsonWindow<'_, F> {
    type Value = bool;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON string window")
    }

    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<bool, E> {
        Ok((self.0)(s))
    }
}

/// Only the last escape can cross a window cut. Twelve bytes cover a surrogate pair;
/// back up over a preceding backslash run so the tail scanner retains its escape parity.
fn complete_json_window_prefix(raw: &[u8]) -> usize {
    let mut start = raw.len().saturating_sub(12);
    while start > 0 && raw[start - 1] == b'\\' {
        start -= 1;
    }
    start + complete_escape_prefix(&raw[start..])
}

/// The longest prefix of `raw` (a JSON string body) that ends on a *complete* escape sequence —
/// i.e. doesn't cut a `\x` or `\uXXXX` in half. Truncating to this length keeps the prefix
/// parseable: the window decoder cuts there so an escape never spans two windows.
pub(crate) fn complete_escape_prefix(raw: &[u8]) -> usize {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] != b'\\' {
            let Some(next) = memchr::memchr(b'\\', &raw[i..]) else {
                break;
            };
            i += next;
        }
        // An escape starts here: 2 bytes (`\n`, `\"`, …), 6 (`\uXXXX`), or 12 (a `\uD8xx\uDCxx`
        // surrogate pair — a cut between the halves would leave an unparseable lone surrogate).
        if raw.get(i + 1) == Some(&b'u') {
            if i + 6 > raw.len() {
                return i; // incomplete at the cut — keep everything before it
            }
            let high_surrogate = std::str::from_utf8(&raw[i + 2..i + 6])
                .ok()
                .and_then(|h| u16::from_str_radix(h, 16).ok())
                .is_some_and(|h| (0xD800..0xDC00).contains(&h));
            let needed = if high_surrogate { 12 } else { 6 };
            if i + needed > raw.len() {
                return i;
            }
            i += needed;
        } else {
            if i + 2 > raw.len() {
                return i;
            }
            i += 2;
        }
    }
    raw.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_decoder_stops_after_one_bounded_window() {
        use std::io::Cursor;
        let body = "\\ud83c\\udf19\\n".repeat(100_000);
        // Independent full-value acceptance establishes the stoppable primitive's precondition.
        let expected: String = serde_json::from_str(&format!("\"{body}\"")).unwrap();
        let mut reader = Cursor::new(body.as_bytes());
        let mut output = String::new();
        decode_body(&mut reader, &mut |text| {
            output.push_str(text);
            false
        })
        .unwrap();
        assert!(reader.position() <= 65_536);
        assert!(reader.position() < body.len() as u64);
        assert!(!output.is_empty());
        assert!(expected.starts_with(&output));
        let mut full_reader = Cursor::new(body.as_bytes());
        let mut full = String::new();
        decode_body(&mut full_reader, &mut |text| {
            full.push_str(text);
            true
        })
        .unwrap();
        assert_eq!(full, expected);
        assert_eq!(full_reader.position(), body.len() as u64);
    }

    #[test]
    fn json_window_tail_matches_complete_escape_scan() {
        let bodies = [
            r#"plain\\\\\\\"\n\u0041\uD834\uDD1Etail"#.to_owned(),
            format!("{}uD834\\uDD1E", "\\".repeat(101)),
            format!("{}uD834\\uDD1E", "\\".repeat(100)),
        ];
        for body in bodies {
            for end in 0..=body.len() {
                assert_eq!(
                    complete_json_window_prefix(&body.as_bytes()[..end]),
                    complete_escape_prefix(&body.as_bytes()[..end]),
                    "{body:?}, cut {end}"
                );
            }
        }
    }

    #[test]
    fn complete_escape_prefix_never_splits_an_escape() {
        // Plain text: the whole slice survives.
        assert_eq!(complete_escape_prefix(b"hello"), 5);
        // Complete escapes survive whole.
        assert_eq!(complete_escape_prefix(b"a\\nb"), 4);
        assert_eq!(complete_escape_prefix(b"a\\u0041b"), 8);
        // A trailing half escape is cut *before* the backslash.
        assert_eq!(complete_escape_prefix(b"abc\\"), 3);
        assert_eq!(complete_escape_prefix(b"abc\\u00"), 3);
        // A high surrogate keeps its partner or is dropped entirely.
        assert_eq!(complete_escape_prefix(b"x\\uD834\\uDD1E"), 13);
        assert_eq!(complete_escape_prefix(b"x\\uD834"), 1);
        assert_eq!(complete_escape_prefix(b"x\\uD834\\uDD"), 1);
        // …and every kept prefix actually unescapes cleanly.
        for raw in [&b"abc\\"[..], b"abc\\u00", b"x\\uD834", b"a\\nb\\"] {
            let keep = complete_escape_prefix(raw);
            let requoted = [b"\"", &raw[..keep], b"\""].concat();
            assert!(
                serde_json::from_slice::<String>(&requoted).is_ok(),
                "prefix of {raw:?} must stay parseable"
            );
        }
    }

    #[test]
    fn preflight_matches_value_validation() {
        let mut cases: Vec<String> = [
            "0",
            "-0",
            "1.5",
            "1e-400",
            "1e400",
            "18446744073709551616",
            "-99999999999999999999999",
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud83c\udf19""#,
            r#""\\ud800""#,
            r#""\\\ud800""#,
            r#"{"\ud800":0}"#,
            r#"{"a":1e400,"a":0}"#,
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        cases.push("9".repeat(1000));
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
            cases.push(format!("{{\"a\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)));
        }
        for input in cases {
            let expected = serde_json::from_str::<serde_json::Value>(&input).is_ok();
            assert_eq!(
                validate_value(&mut input.as_bytes(), true, 0, |_, _, _| {}).is_some(),
                expected,
                "bytes: {input}"
            );
            assert_eq!(
                validate_value(&mut Text(input.as_bytes()), true, 0, |_, _, _| {}).is_some(),
                expected,
                "text: {input}"
            );
            // Grammar alone accepts what RawValue accepts.
            assert_eq!(
                validate_value(&mut Text(input.as_bytes()), false, 0, |_, _, _| {}).is_some(),
                serde_json::from_str::<&crate::lazy::RawValue>(&input).is_ok(),
                "syntax: {input}"
            );
        }
    }

    /// The one-pass validator is the record acceptance boundary: it must agree with Value on
    /// every input, including near-misses of valid documents.
    #[test]
    fn validate_value_accepts_exactly_what_value_accepts() {
        let mut cases: Vec<String> = [
            "",
            " ",
            "0",
            "-0",
            "01",
            "-",
            "1.",
            ".5",
            "1e",
            "1e+",
            "1E5",
            "-1.5e-3",
            "1e400",
            "18446744073709551616",
            "true",
            "tru",
            "truex",
            "null",
            "nul",
            "false ",
            " \t\r\n{}\n",
            "{}",
            "[]",
            "[,]",
            "[1,]",
            "{,}",
            r#"{"a":}"#,
            r#"{"a"}"#,
            r#"{"a":1,}"#,
            r#"{"a":1 "b":2}"#,
            r#"{1:2}"#,
            r#"[1 2]"#,
            "[1}",
            "{]",
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud83c\udf19""#,
            r#""\\ud800""#,
            r#""\\\ud800""#,
            r#""\ud800\u0041""#,
            r#""\x""#,
            r#""\u12""#,
            r#""\u12g4""#,
            "\"tab\tinside\"",
            "\"nl\ninside\"",
            "\"\u{7f}\"",
            "\"é🌙\"",
            r#"{"\ud800":0}"#,
            r#"{"a":1e400,"a":0}"#,
            r#"{"a":1,"a":2}"#,
            "[true,false,null]",
            "[]]",
            "[[]",
            "\"",
            r#""unterminated"#,
            r#""a"x"#,
            "{} {}",
            "[1] x",
            "\u{a0}1",
            "[-]",
            "[--1]",
            "[1.5.5]",
            "[0x10]",
            "[+1]",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        cases.push("9".repeat(1000));
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
            cases.push(format!("{{\"a\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)));
        }
        // Mutations of a realistic record: every single-byte deletion, and replacements by bytes
        // that matter to the grammar.
        let record = r#"{"type":"user","n":-12.5e2,"ok":true,"z":null,"m":{"c":[{"t":"a\"b\\c\u00e9\ud83c\udf19","x":[1,2,[]]}],"e":{}}}"#;
        for at in 0..record.len() {
            let mut deleted = record.as_bytes().to_vec();
            deleted.remove(at);
            cases.extend(String::from_utf8(deleted).ok());
            for byte in [
                b'"', b'\\', b',', b':', b'{', b'}', b'[', b']', b' ', b'0', b'e', b'-', b'u', 0x01,
            ] {
                let mut replaced = record.as_bytes().to_vec();
                replaced[at] = byte;
                cases.extend(String::from_utf8(replaced).ok());
            }
        }
        for input in cases {
            let oracle = serde_json::from_str::<serde_json::Value>(&input).is_ok();
            let bounds = validate_value(&mut Text(input.as_bytes()), true, 0, |_, _, _| {});
            assert_eq!(bounds.is_some(), oracle, "{input:?}");
            if let Some((start, end)) = bounds {
                assert_eq!(&input[start..end], trim_str(&input), "{input:?}");
            }
            // A child value validated with its parents' levels open sees the same limit.
            let nested = format!("[{input}]");
            assert_eq!(
                validate_value(&mut Text(input.as_bytes()), true, 1, |_, _, _| {}).is_some(),
                serde_json::from_str::<serde_json::Value>(&nested).is_ok() && oracle,
                "nested {input:?}"
            );
        }
    }

    #[test]
    fn rejects_non_utf8_ignored_text() {
        assert!(validate_value(&mut &b"{\"ignored\":\"\xff\"}"[..], true, 0, |_, _, _| {}).is_none());
        assert!(validate_value(&mut &b"{\"ignored\":\"\xff\"}"[..], false, 0, |_, _, _| {}).is_none());
    }
}
