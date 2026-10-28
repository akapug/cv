//! Anchors: a sparse fingerprint of a conversation that survives the harness's request rewriting.
//!
//! A harness does not send its transcript verbatim. At compaction time Claude Code truncates tool
//! output, strips images, injects reminders, merges a prompt with the tool results beside it and
//! moves cache markers; a hash of the whole history would miss nearly every time. What it does not
//! rewrite is the prose: what the person typed and what the model said. So the fingerprint is that
//! prose, cut into segments at harness markup, each segment whitespace-normalized and kept as a
//! length and two hashes, never as text.
//!
//! Tool calls add the other thing a harness never rewrites: the call id the model's provider gave
//! each call, echoed in every later request. A call id is an anchor in the same stream, as a
//! [`call_marker`], so a window of tool work with little prose still pins where its cut is.
//!
//! A request matches when every anchor occurs, in order, as a substring of the request's own
//! normalized prose. Substring, not equality, because a harness may append a reminder to a prompt
//! or join two blocks; in order, because a rewound or edited history moves or drops some.
//!
//! The search is Rabin-Karp: each anchor's rolling hash is compared at each position after the
//! previous match, and a hit is confirmed by SHA-256. The scans of consecutive anchors cover
//! disjoint stretches of the request, so a whole match costs O(request prose + anchor bytes).

use serde::{Deserialize, Serialize};

/// A prose segment shorter than this identifies nothing ("ok", "yes") and is not an anchor. A
/// representation constant: it changes how much of a session the fingerprint uses, never whether a
/// served summary is current (a shorter fingerprint can only miss more).
pub const MIN_ANCHOR_BYTES: usize = 12;

/// One anchor: a normalized prose segment's length, rolling hash and SHA-256 prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Bytes.
    pub n: u32,
    /// The polynomial rolling hash ([`roll`]).
    pub h: u64,
    /// The first 16 hex digits of the segment's SHA-256.
    pub s: String,
}

impl Anchor {
    pub fn of(segment: &str) -> Anchor {
        Anchor {
            n: u32::try_from(segment.len()).unwrap_or(u32::MAX),
            h: roll(segment.as_bytes()),
            s: short_sha(segment.as_bytes()),
        }
    }
}

fn short_sha(bytes: &[u8]) -> String {
    let mut s = crate::digest::sha256_hex(bytes);
    s.truncate(16);
    s
}

const MODULUS: u64 = (1 << 61) - 1;
const BASE: u64 = 1_000_003;

fn mul(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    let folded = (product & u128::from(MODULUS)) + (product >> 61);
    let folded = folded as u64;
    if folded >= MODULUS {
        folded - MODULUS
    } else {
        folded
    }
}

fn add(a: u64, b: u64) -> u64 {
    let sum = a + b;
    if sum >= MODULUS {
        sum - MODULUS
    } else {
        sum
    }
}

fn sub(a: u64, b: u64) -> u64 {
    if a >= b {
        a - b
    } else {
        a + MODULUS - b
    }
}

fn pow(mut base: u64, mut exp: u64) -> u64 {
    let mut out = 1;
    while exp > 0 {
        if exp & 1 == 1 {
            out = mul(out, base);
        }
        base = mul(base, base);
        exp >>= 1;
    }
    out
}

/// The polynomial hash of `bytes` modulo 2^61-1: Σ b[i]·BASE^(n-1-i).
pub fn roll(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |h, &b| add(mul(h, BASE), u64::from(b) + 1))
}

/// Collapse every whitespace run to one space and trim: the one normalization both sides share.
pub fn normalize_into(out: &mut String, text: &str) {
    let mut pending_space = !out.is_empty();
    for word in text.split_whitespace() {
        if pending_space {
            out.push(' ');
        }
        out.push_str(word);
        pending_space = true;
    }
}

/// A tool call's place in the anchor stream. The delimiters keep one id from matching inside a
/// longer one; a control character never occurs in normalized prose, so a marker never matches
/// text. `None` for an id cv derived itself (`cv:<format>:<position>`), which no request carries.
pub fn call_marker(id: &str) -> Option<String> {
    (!id.is_empty() && !id.starts_with("cv:") && !id.chars().any(char::is_whitespace))
        .then(|| format!("\u{1}call {id}\u{1}"))
}

/// `text`'s prose segments: the text with harness markup elements cut out (an element is
/// `<name ...>…</name>` with a lowercase name such as `system-reminder` or `task-notification`),
/// each remaining piece normalized, pieces under [`MIN_ANCHOR_BYTES`] dropped. Markup is cut by
/// shape, not by a list of tag names, so a new harness's markup is cut too; a person's own markup
/// is cut as well, which only shortens the fingerprint.
pub fn segments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |piece: &str| {
        let mut s = String::new();
        normalize_into(&mut s, piece);
        if s.len() >= MIN_ANCHOR_BYTES {
            out.push(s);
        }
    };
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) = element_end(text, i) {
                push(&text[start..i]);
                start = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    push(&text[start..]);
    out
}

/// When an element `<name ...>…</name>` opens at `at`, the byte after its close tag.
fn element_end(text: &str, at: usize) -> Option<usize> {
    let rest = &text[at + 1..];
    let name_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
        .count();
    if name_len == 0 || !rest.as_bytes()[0].is_ascii_lowercase() {
        return None;
    }
    let name = &rest[..name_len];
    let after = rest.as_bytes().get(name_len)?;
    if *after != b'>' && !after.is_ascii_whitespace() {
        return None;
    }
    let open_end = rest.find('>')? + 1;
    let close = format!("</{name}>");
    let close_at = rest[open_end..].find(&close)?;
    Some(at + 1 + open_end + close_at + close.len())
}

/// Find each anchor in `stream` (normalized prose), in order, each after the previous one's end.
/// `Ok` holds each match's end offset; `Err` the index of the first anchor not found.
pub fn find_in_order(stream: &[u8], anchors: &[Anchor]) -> Result<Vec<usize>, usize> {
    let mut ends = Vec::with_capacity(anchors.len());
    let mut from = 0usize;
    for (index, anchor) in anchors.iter().enumerate() {
        match find_from(stream, anchor, from) {
            Some(end) => {
                ends.push(end);
                from = end;
            }
            None => return Err(index),
        }
    }
    Ok(ends)
}

fn find_from(stream: &[u8], anchor: &Anchor, from: usize) -> Option<usize> {
    let n = anchor.n as usize;
    if n == 0 || from.checked_add(n)? > stream.len() {
        return None;
    }
    let high = pow(BASE, (n - 1) as u64);
    let mut h = roll(&stream[from..from + n]);
    let mut at = from;
    loop {
        if h == anchor.h && short_sha(&stream[at..at + n]) == anchor.s {
            return Some(at + n);
        }
        if at + n >= stream.len() {
            return None;
        }
        let out = u64::from(stream[at]) + 1;
        let incoming = u64::from(stream[at + n]) + 1;
        h = add(mul(sub(h, mul(out, high)), BASE), incoming);
        at += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_elements_are_cut_by_shape_and_pieces_normalized() {
        let text = "Fix the   parser\nplease <system-reminder>\nnoise\n</system-reminder> and then the writer too";
        assert_eq!(segments(text), vec!["Fix the parser please", "and then the writer too"]);
        // Not an element: an unclosed tag, an uppercase name, a comparison.
        assert_eq!(
            segments("if a <b then c <Div>x</Div> end"),
            vec!["if a <b then c <Div>x</Div> end"]
        );
        // Short pieces are dropped.
        assert!(segments("ok").is_empty());
        assert!(segments("<task-notification><id>1</id></task-notification>").is_empty());
    }

    #[test]
    fn rolling_matches_direct_hash_everywhere() {
        let stream = b"the quick brown fox jumps over the lazy dog, the quick brown fox again";
        for n in [1usize, 5, 12, 20] {
            for at in 0..=stream.len() - n {
                let anchor = Anchor {
                    n: n as u32,
                    h: roll(&stream[at..at + n]),
                    s: short_sha(&stream[at..at + n]),
                };
                let end = find_from(stream, &anchor, 0).unwrap();
                // The first occurrence, which is at or before `at`.
                assert!(end - n <= at);
                assert_eq!(&stream[end - n..end], &stream[at..at + n]);
            }
        }
    }

    #[test]
    fn anchors_must_appear_in_order() {
        let a = Anchor::of("first prompt here");
        let b = Anchor::of("second reply here");
        let stream = b"x first prompt here y second reply here z";
        assert_eq!(find_in_order(stream, &[a.clone(), b.clone()]).unwrap().len(), 2);
        let swapped = b"x second reply here y first prompt here z";
        assert_eq!(find_in_order(swapped, &[a.clone(), b.clone()]), Err(1));
        assert_eq!(find_in_order(b"nothing", &[a]), Err(0));
    }

    #[test]
    fn a_rolling_hash_collision_is_rejected_by_the_digest() {
        let stream = b"aaaaaaaaaaaaaaaaaaaa";
        let mut anchor = Anchor::of("aaaaaaaaaaaa");
        anchor.s = "0000000000000000".into();
        assert_eq!(find_from(stream, &anchor, 0), None);
    }
}
