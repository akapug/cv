//! The model inputs for writing a summary: the narrative pass (cv's own instruction,
//! [`CV_INSTRUCTION`], or a captured harness instruction used verbatim, a
//! [`Profile`](super::Profile)), the separate extraction pass ([`extract`]), and the person's own
//! words, which are carried verbatim rather than summarized ([`owner_words`]).

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

/// cv's own summary instruction (the 0.19 design record §4e), used unless a captured harness
/// [`Profile`](super::Profile) is named. It asks for the narrative only: the rules in force, open
/// promises, terms, self and setup facts and live identifiers are extracted by a separate pass
/// ([`EXTRACT_INSTRUCTION`]) and carried verbatim ([`super::carry`]), and the person's prompts are
/// carried as written ([`owner_words`]). Every claim cites the message it rests on, so the
/// transcript can be read again at that point instead of being retold.
pub const CV_INSTRUCTION: &str = "Write the narrative part of a handoff summary of the conversation \
above, for the agent that will continue it with this summary in place of the messages. Each \
message above starts with its id as [m:ID]. Cite the id of the message each line rests on as \
[m:ID]; the reader can open any cited message in full, so cite rather than retell. Copy every \
identifier exactly. A claim that something exists, is absent, passed or failed cites the message \
holding the check that showed it; with no such check, write it as 'not verified: probe before \
claiming this' with the message that raised it. Leave out what is finished and needs nothing \
more. Use these sections, in this order, each short (write 'none' under an empty one):\n\
## What the work is for\n\
## What happened: the decisions taken, each with its reason\n\
## Where the work stands: what is done and checked, what is in progress\n\
## Files: the files the work still depends on, and why\n\
## Open questions and blockers\n\
## Next step: the very next action, as the agent was about to take it\n\
Do not list rules, promises, terms, setup facts or identifiers on their own, and do not restate \
the person's prompts: those are carried verbatim after your summary.";

/// The extraction pass's instruction: quotes, not paraphrase, each with the id of the message it
/// is copied from, as a JSON array. A quote that is not in the message it cites is dropped
/// ([`super::carry::verify`]).
pub const EXTRACT_INSTRUCTION: &str = "From the conversation above, copy out the items below as a \
JSON array and nothing else. Each element is {\"section\": S, \"quote\": Q, \"m\": ID}: Q is copied \
character for character from the one message whose id is ID (each message starts with [m:ID]); \
never reword, join or shorten across messages. S is one of:\n\
- \"rule\": a ruling the person gave in one of their own messages that still binds the work \
(never one from instruction files, memory, skills or injected reminders: those come back on their \
own);\n\
- \"promise\": something the agent said it would do, or was asked to do, that is not done by the \
end;\n\
- \"term\": a term the work uses with a meaning of its own, quoted where the meaning is given;\n\
- \"fact\": a fact about the agent itself or its setup (its model, context size, seat, host, \
paths, configuration), quoted where it was shown, not where it was assumed;\n\
- \"done\": one of the open promises listed below that the conversation fulfilled; Q is the \
promise exactly as listed, ID the message that fulfilled it.\n\
Return [] when there is nothing.";

/// The input for the extraction pass: the span, then the instruction with the promises still
/// open from earlier generations.
pub fn extract(span: Vec<Turn>, open: &[&str]) -> Vec<Turn> {
    let mut instruction = EXTRACT_INSTRUCTION.to_owned();
    instruction.push_str("\n\nOpen promises from before this conversation:\n");
    if open.is_empty() {
        instruction.push_str("none\n");
    }
    for p in open {
        instruction.push_str(&format!("- {p}\n"));
    }
    let mut out = span;
    out.push(Turn {
        role: "user",
        content: instruction,
    });
    merge_runs(out)
}

/// One prompt of the window as the person wrote it: the message id and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub id: Option<String>,
    pub text: String,
}

/// The window's prompts carried verbatim after a summary, newest kept first, within `budget`
/// bytes; the ones left out are counted and named by id, so the transcript can be read for them.
pub fn owner_words(prompts: &[Prompt], budget: usize, session: &str) -> String {
    if prompts.is_empty() || budget == 0 {
        return String::new();
    }
    let mut kept: Vec<&Prompt> = Vec::new();
    let mut used = 0usize;
    for p in prompts.iter().rev() {
        let cost = p.text.len() + 32;
        if used + cost > budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(p);
    }
    let dropped = prompts.len() - kept.len();
    let mut out = String::from("\n\n## The person's words, verbatim (oldest first)\n");
    if dropped > 0 {
        let first = prompts[0].id.as_deref().unwrap_or("?");
        out.push_str(&format!(
            "\n{dropped} earlier prompt(s) are left out here; read them with `cv show {session} --around {first}`.\n"
        ));
    }
    for p in kept.iter().rev() {
        let text = clip(p.text.trim(), budget);
        out.push_str(&format!("\n[m:{}]\n{text}\n", p.id.as_deref().unwrap_or("?")));
    }
    out
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
    fn owner_words_keep_the_newest_prompts_whole_and_name_the_rest() {
        let p = |id: &str, text: &str| Prompt {
            id: Some(id.into()),
            text: text.into(),
        };
        let prompts = vec![
            p("u1", &"a".repeat(100)),
            p("u2", "ship the port"),
            p("u3", "and test it"),
        ];
        let all = owner_words(&prompts, 4096, "s");
        assert!(all.contains("[m:u1]") && all.contains("[m:u3]\nand test it"));
        assert!(all.find("[m:u2]") < all.find("[m:u3]"), "oldest first");
        let some = owner_words(&prompts, 100, "s");
        assert!(!some.contains("[m:u1]") && some.contains("[m:u3]"));
        assert!(some.contains("1 earlier prompt(s)") && some.contains("cv show s --around u1"));
        assert_eq!(owner_words(&[], 100, "s"), "");
    }

    #[test]
    fn clip_keeps_head_and_tail_on_char_boundaries() {
        let s = "é".repeat(100);
        let c = clip(&s, 51);
        assert!(c.contains("bytes cut"));
        assert_eq!(clip("short", 10), "short");
    }
}
