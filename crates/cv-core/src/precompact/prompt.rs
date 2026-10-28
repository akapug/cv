//! The model inputs for writing and finishing a summary. The harness's own instruction is used
//! verbatim (from its [`Profile`](super::Profile)); cv adds only plain framing of its own.

use super::request;

/// One chat message for the summary model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `user` or `assistant`.
    pub role: &'static str,
    pub content: String,
}

/// The input that writes a window's summary up to a cut: an earlier summary when extending (the
/// same way a harness's own window starts with its previous summary), the span since it, then
/// the harness's instruction. Each extension reads only its span, so the cost of keeping a
/// summary current grows with the new tokens, not with the session.
pub fn write(prior: Option<&str>, span: Vec<Turn>, instruction: &str) -> Vec<Turn> {
    let mut out = Vec::with_capacity(span.len() + 2);
    if let Some(prior) = prior {
        out.push(Turn {
            role: "user",
            content: format!("[Summary of the conversation before the messages that follow]\n\n{prior}"),
        });
    }
    out.extend(span);
    out.push(Turn {
        role: "user",
        content: instruction.to_owned(),
    });
    merge_runs(out)
}

/// cv's own instruction for the answer-time pass over a tail.
pub const TAIL_INSTRUCTION: &str =
    "Below is a summary of a conversation, followed by the messages that came after it. \
Write only an update covering those later messages: what was asked, what was done, files and \
decisions involved, errors and fixes, and the work in progress at the very end with the next \
step. Use short markdown sections. Do not repeat what the summary already says.";

/// The input for the answer-time pass: the stored summary and the request's tail, each tool
/// note cut to `note_bytes` (head and tail kept, the cut named).
pub fn tail(summary: &str, tail: &[request::Message], note_bytes: usize) -> Vec<Turn> {
    let mut rendered = String::new();
    for m in tail {
        rendered.push_str(&format!("[{}]", m.role));
        for t in &m.texts {
            rendered.push('\n');
            rendered.push_str(t);
        }
        for n in &m.notes {
            rendered.push('\n');
            rendered.push_str(&clip(n, note_bytes));
        }
        rendered.push_str("\n\n");
    }
    vec![Turn {
        role: "user",
        content: format!(
            "{TAIL_INSTRUCTION}\n\n<summary-so-far>\n{summary}\n</summary-so-far>\n\n<later-messages>\n{rendered}</later-messages>"
        ),
    }]
}

/// Join a summary and the update for its tail. Inside the summary's closing `</summary>` tag when
/// it has one (Claude Code keeps only what that element holds), else after it.
pub fn splice(summary: &str, update: &str) -> String {
    let update = update.trim();
    if update.is_empty() {
        return summary.to_owned();
    }
    let section = format!("\n\n## Since the summary above\n\n{update}\n");
    match summary.rfind("</summary>") {
        Some(at) => format!("{}{section}{}", &summary[..at], &summary[at..]),
        None => format!("{summary}{section}"),
    }
}

/// Merge consecutive same-role turns (providers want alternation).
fn merge_runs(turns: Vec<Turn>) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::with_capacity(turns.len());
    for t in turns {
        match out.last_mut() {
            Some(last) if last.role == t.role => {
                last.content.push_str("\n\n");
                last.content.push_str(&t.content);
            }
            _ => out.push(t),
        }
    }
    out
}

/// Cut `text` to `max` bytes, keeping its head and tail around a marker that names the cut.
pub fn clip(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if text.len() <= max {
        return text.into();
    }
    let half = max / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n[… {} bytes cut …]\n{}", &text[..head], tail - head, &text[tail..]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_puts_the_prior_first_and_the_instruction_last_alternating() {
        let span = vec![
            Turn {
                role: "user",
                content: "a".into(),
            },
            Turn {
                role: "user",
                content: "b".into(),
            },
            Turn {
                role: "assistant",
                content: "c".into(),
            },
        ];
        let turns = write(Some("S"), span, "INSTR");
        assert_eq!(turns.len(), 3);
        assert!(turns[0].content.starts_with("[Summary") && turns[0].content.ends_with("a\n\nb"));
        assert_eq!(turns[2].content, "INSTR");
    }

    #[test]
    fn splice_lands_inside_the_summary_element() {
        let s = "<analysis>x</analysis>\n<summary>\n1. Intent\n</summary>";
        let joined = splice(s, "did more");
        assert!(joined.ends_with("did more\n</summary>"));
        assert_eq!(splice("plain", "  "), "plain");
        assert!(splice("plain", "u").starts_with("plain\n\n## Since"));
    }

    #[test]
    fn clip_keeps_head_and_tail_on_char_boundaries() {
        let s = "é".repeat(100);
        let c = clip(&s, 51);
        assert!(c.contains("bytes cut"));
        assert_eq!(clip("short", 10), "short");
    }
}
