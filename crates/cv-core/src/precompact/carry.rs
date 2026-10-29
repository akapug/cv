//! What a summary carries verbatim from one generation to the next, never re-summarized: the
//! person's rulings, the open promises, the terms the work uses, the self and setup facts, the
//! live identifiers, and pointers to the standing rules the work touched.
//!
//! Rules come in three layers, and cv re-introduces only the middle one. The base layer
//! (instruction files, memory, skills) is re-injected by the harness after every compaction, so it
//! is never copied. The per-turn layer (rules a hook injects when a turn calls for them) keeps
//! firing after the seam, so it is not duplicated either. What a compaction loses is the
//! contextual layer: the person's rulings in the span, verbatim (a ruling that was never filed
//! anywhere exists only there), and which standing rules the span's work touched, carried as
//! pointers ([`Pointer`]: the rule's id, never its text). The selection is deterministic: a
//! ruling must quote a message the person wrote, outside the harness's markup, and a pointer is an
//! id the configured patterns found in the span; each records why it was chosen.
//!
//! Loss compounds over summary generations: a compactor asked to keep side constraints keeps few
//! of them, while a separate extractor whose output is appended after the summary keeps most
//! (arXiv 2608.11242). So the narrative summary and these sections are two passes: the extraction
//! pass returns quotes, each quote is kept only when it is found verbatim in the message it cites
//! ([`verify`], so an extractor cannot plant a fact the transcript does not hold), and the kept
//! items are merged into the previous generation's ([`Carry::merge`]) instead of being
//! summarized again. Identifiers are read from the span by shape ([`ids`]), with no model.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which carried section an item belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    /// An instruction from the person (or their configuration) that still binds the work.
    Rule,
    /// Something the agent said it would do, or was asked to do, not yet done.
    Promise,
    /// A term the work uses with a meaning of its own, and that meaning.
    Term,
    /// A fact about the agent itself or its setup (model, context size, seat, host, paths).
    Fact,
}

impl Section {
    const ALL: [Section; 4] = [Section::Rule, Section::Promise, Section::Term, Section::Fact];

    fn heading(self) -> &'static str {
        match self {
            Section::Rule => "The person's rulings in force",
            Section::Promise => "Open promises",
            Section::Term => "Terms",
            Section::Fact => "Self and setup facts",
        }
    }
}

/// One carried item: a quote, verbatim from the message `m` cites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub section: Section,
    pub quote: String,
    /// The id of the message the quote is from.
    pub m: String,
    /// Why it is carried: said in which generation's span.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// A standing rule the span's work touched: its id as the configured patterns found it (a rule
/// store id, an instruction name), where it was last seen and how often. The text is never
/// copied: the reader looks the id up where such rules live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub id: String,
    pub last: String,
    pub hits: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// An identifier seen in the span (a commit sha, a task row, a path, a URL), with the first and
/// the last message it appears in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Id {
    pub id: String,
    pub first: String,
    pub last: String,
}

/// What one span adds to the carry, before the merge.
#[derive(Debug, Clone, Default)]
pub struct Span {
    pub items: Vec<Item>,
    pub ids: Vec<Id>,
    pub pointers: Vec<Pointer>,
}

/// The carried sections of one record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carry {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pointers: Vec<Pointer>,
}

/// What the extraction pass returned that was not kept: quotes not found verbatim in the message
/// they cite, and entries that were not items at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped {
    pub unverified: usize,
    pub malformed: usize,
    /// Quotes from the layers cv does not re-introduce: a ruling not in a message the person
    /// wrote (or inside harness markup there), any item quoting injected context.
    pub outside_layer: usize,
}

/// An extraction entry as the model writes it: `{"section", "quote", "m"}`, where `section` may
/// also be `done` (a prior promise, quoted, that the span fulfilled).
#[derive(Debug, Deserialize)]
struct Raw {
    section: String,
    quote: String,
    m: String,
}

/// The extraction pass's answer: the first JSON array in `text` (a model may wrap it in prose or
/// a code fence). Returns the items and the quotes of promises marked done.
pub fn parse(text: &str) -> (Vec<Item>, Vec<String>, Dropped) {
    let mut dropped = Dropped::default();
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return (Vec::new(), Vec::new(), dropped);
    };
    if end < start {
        return (Vec::new(), Vec::new(), dropped);
    }
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end]) else {
        dropped.malformed += 1;
        return (Vec::new(), Vec::new(), dropped);
    };
    let (mut items, mut done) = (Vec::new(), Vec::new());
    for v in values {
        let Ok(raw) = serde_json::from_value::<Raw>(v) else {
            dropped.malformed += 1;
            continue;
        };
        let section = match raw.section.as_str() {
            "rule" => Section::Rule,
            "promise" => Section::Promise,
            "term" => Section::Term,
            "fact" => Section::Fact,
            "done" => {
                done.push(raw.quote);
                continue;
            }
            _ => {
                dropped.malformed += 1;
                continue;
            }
        };
        items.push(Item {
            section,
            quote: raw.quote,
            m: raw.m,
            why: String::new(),
        });
    }
    (items, done, dropped)
}

fn normalized(text: &str) -> String {
    let mut out = String::new();
    super::anchor::normalize_into(&mut out, text);
    out
}

/// Which messages of the span are whose: the person's prompts, context the harness or a hook
/// injected, and the agent's own replies.
#[derive(Debug, Clone, Default)]
pub struct Layers {
    pub prompts: std::collections::HashSet<String>,
    pub injected: std::collections::HashSet<String>,
    /// The agent's replies: a self or setup fact quoted from one is what the agent assumed, not
    /// where the fact was shown, so it is not carried.
    pub replies: std::collections::HashSet<String>,
}

/// Keep only the items whose quote is in the message they cite, whitespace aside, and from the
/// layer cv re-introduces: no item quotes injected context, a ruling quotes a prompt the person
/// wrote, outside the harness's markup in it, and a self or setup fact never quotes the agent. `texts` maps a message id to its text as the
/// span holds it.
pub fn verify(items: Vec<Item>, texts: &HashMap<String, String>, layers: &Layers, dropped: &mut Dropped) -> Vec<Item> {
    items
        .into_iter()
        .filter(|item| {
            let ruling = item.section == Section::Rule;
            if layers.injected.contains(&item.m)
                || ruling && !layers.prompts.contains(&item.m)
                || item.section == Section::Fact && layers.replies.contains(&item.m)
            {
                dropped.outside_layer += 1;
                return false;
            }
            let quote = normalized(&item.quote);
            let found = !quote.is_empty()
                && texts.get(&item.m).is_some_and(|t| {
                    let t = if ruling {
                        super::anchor::without_markup(t)
                    } else {
                        t.clone()
                    };
                    normalized(&t).contains(&quote)
                });
            if !found {
                dropped.unverified += 1;
            }
            found
        })
        .collect()
}

/// The pointers `found` (`(message id, rule id)`, oldest first) make: one per rule id, with its
/// last message and count.
pub fn pointers(found: &[(String, String)]) -> Vec<Pointer> {
    let mut out: Vec<Pointer> = Vec::new();
    for (m, id) in found {
        match out.iter_mut().find(|p| &p.id == id) {
            Some(p) => {
                p.last = m.clone();
                p.hits += 1;
            }
            None => out.push(Pointer {
                id: id.clone(),
                last: m.clone(),
                hits: 1,
                why: String::new(),
            }),
        }
    }
    out
}

/// The identifiers in `messages` (`(id, text)`, oldest first) by shape: commit shas (7 to 40 hex
/// digits holding a digit and a letter), task rows (`task/N`), absolute paths, URLs. Each with
/// the first and last message it appears in, in order of first appearance.
pub fn ids(messages: &[(String, String)]) -> Vec<Id> {
    use std::sync::OnceLock;
    static SHAPE: OnceLock<regex::Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            https?://[^\s<>()\[\]`'\x22]+
          | (?:^|[\s`'\x22(\[])(/(?:[A-Za-z0-9._@+-]+/)+[A-Za-z0-9._@+-]*)
          | \btask/[0-9]+\b
          | \b[0-9a-f]{7,40}\b",
        )
        .expect("the id shapes compile")
    });
    let mut out: Vec<Id> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (m, text) in messages {
        for c in shape.captures_iter(text) {
            let id = c.get(1).or_else(|| c.get(0)).map_or("", |g| g.as_str());
            let id = id.trim_end_matches(['.', ',', ':', ';']);
            let hex = id.bytes().all(|b| b.is_ascii_hexdigit());
            if id.is_empty()
                || hex && !(id.bytes().any(|b| b.is_ascii_digit()) && id.bytes().any(|b| b.is_ascii_alphabetic()))
            {
                continue;
            }
            match at.get(id) {
                Some(&i) => out[i].last = m.clone(),
                None => {
                    at.insert(id.to_owned(), out.len());
                    out.push(Id {
                        id: id.to_owned(),
                        first: m.clone(),
                        last: m.clone(),
                    });
                }
            }
        }
    }
    out
}

impl Carry {
    /// The next generation: this one's items and ids, then the span's new ones (an item already
    /// carried, whitespace aside, is not repeated), less the promises `done` quotes; the id list
    /// keeps the `max_ids` most recently seen. Nothing carried is rewritten.
    pub fn merge(&self, span: Span, done: &[String], max_ids: usize) -> Carry {
        let Span { items, ids, pointers } = span;
        let done: Vec<String> = done.iter().map(|d| normalized(d)).collect();
        let mut seen: std::collections::HashSet<(Section, String)> = std::collections::HashSet::new();
        let mut next = Vec::new();
        let fresh = items.into_iter().map(|mut i| {
            i.why = format!("said in this span at [m:{}]", i.m);
            i
        });
        let carried = self.items.iter().cloned().map(|mut i| {
            if !i.why.starts_with("carried") {
                i.why = format!("carried from an earlier summary; first said at [m:{}]", i.m);
            }
            i
        });
        for item in carried.chain(fresh) {
            let key = (item.section, normalized(&item.quote));
            if item.section == Section::Promise && done.iter().any(|d| d == &key.1) {
                continue;
            }
            if seen.insert(key) {
                next.push(item);
            }
        }
        // An id seen again moves to the end: the list runs from least to most recently seen.
        let mut all = self.ids.clone();
        for mut id in ids {
            if let Some(at) = all.iter().position(|i| i.id == id.id) {
                id.first = all.remove(at).first;
            }
            all.push(id);
        }
        if all.len() > max_ids {
            all.drain(..all.len() - max_ids);
        }
        let mut rules: Vec<Pointer> = self
            .pointers
            .iter()
            .cloned()
            .map(|mut p| {
                p.why = format!("carried: the work touched it earlier, last at [m:{}]", p.last);
                p
            })
            .collect();
        for mut p in pointers {
            p.why = format!(
                "the work in this span touched it {} time(s), last at [m:{}]",
                p.hits, p.last
            );
            match rules.iter().position(|r| r.id == p.id) {
                Some(at) => rules[at] = p,
                None => rules.push(p),
            }
        }
        Carry {
            items: next,
            ids: all,
            pointers: rules,
        }
    }

    /// The carried sections as the summary's tail: each quote verbatim with the message it is
    /// from, the standing rules the work touched as pointers, and the identifiers with where they
    /// were last seen.
    pub fn render(&self, session: &str) -> String {
        let mut out = String::new();
        for section in Section::ALL {
            let items: Vec<&Item> = self.items.iter().filter(|i| i.section == section).collect();
            out.push_str(&format!("\n\n## {} (verbatim, carried)\n", section.heading()));
            if items.is_empty() {
                out.push_str("\nnone\n");
            }
            for item in items {
                out.push_str(&format!("\n- [m:{}] {}", item.m, item.quote.trim()));
            }
            out.push('\n');
        }
        if !self.pointers.is_empty() {
            out.push_str(
                "\n\n## Standing rules this work touched (pointers: look each id up where your rules \
                 live; their text is not copied, and rules the harness re-injects are not listed)\n\n",
            );
            for p in &self.pointers {
                out.push_str(&format!("- `{}` ({})\n", p.id, p.why));
            }
        }
        if !self.ids.is_empty() {
            out.push_str(&format!(
                "\n\n## Live identifiers (seen in the transcript; not verified now: probe before claiming one \
                 exists or is absent; `cv show {session} --around ID` opens where each was last seen)\n\n"
            ));
            for id in &self.ids {
                out.push_str(&format!("- `{}` [m:{}]\n", id.id, id.last));
            }
        }
        out
    }

    /// The open promises, for the extraction pass to mark done.
    pub fn promises(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.section == Section::Promise)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_is_kept_only_where_its_message_holds_it() {
        let (items, done, mut dropped) = parse(
            "Here they are:\n```json\n[\
             {\"section\":\"rule\",\"quote\":\"never  push main\",\"m\":\"u1\"},\
             {\"section\":\"fact\",\"quote\":\"the seat has a 1M context\",\"m\":\"u1\"},\
             {\"section\":\"done\",\"quote\":\"I will run the tests\",\"m\":\"a2\"},\
             {\"section\":\"mood\",\"quote\":\"x\",\"m\":\"u1\"}]\n```",
        );
        assert_eq!(done, vec!["I will run the tests".to_owned()]);
        assert_eq!(dropped.malformed, 1);
        let texts = HashMap::from([("u1".to_owned(), "Rule: never push\nmain, ever.".to_owned())]);
        let layers = Layers {
            prompts: ["u1".to_owned()].into(),
            ..Default::default()
        };
        let kept = verify(items, &texts, &layers, &mut dropped);
        assert_eq!(kept.len(), 1, "the planted fact is not in u1");
        assert_eq!(kept[0].section, Section::Rule);
        assert_eq!(dropped.unverified, 1);
    }

    #[test]
    fn a_ruling_quotes_the_person_and_nothing_quotes_injected_context() {
        let rule = |m: &str, quote: &str| Item {
            section: Section::Rule,
            quote: quote.into(),
            m: m.into(),
            why: String::new(),
        };
        let texts = HashMap::from([
            (
                "u1".to_owned(),
                "keep it small <system-reminder>never push main</system-reminder>".to_owned(),
            ),
            (
                "a1".to_owned(),
                "I will never push main. I run with a 1M context.".to_owned(),
            ),
            ("r1".to_owned(), "CLAUDE.md: never push main".to_owned()),
            ("t1".to_owned(), "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000".to_owned()),
        ]);
        let layers = Layers {
            prompts: ["u1".to_owned()].into(),
            injected: ["r1".to_owned()].into(),
            replies: ["a1".to_owned()].into(),
        };
        let mut dropped = Dropped::default();
        let fact = |m: &str, quote: &str| Item {
            section: Section::Fact,
            ..rule(m, quote)
        };
        let kept = verify(
            vec![
                rule("u1", "keep it small"),
                rule("u1", "never push main"),
                rule("a1", "never push main"),
                fact("r1", "never push main"),
                fact("a1", "I run with a 1M context"),
                fact("t1", "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000"),
            ],
            &texts,
            &layers,
            &mut dropped,
        );
        assert_eq!(
            kept,
            vec![
                rule("u1", "keep it small"),
                fact("t1", "CLAUDE_CODE_AUTO_COMPACT_WINDOW=400000")
            ]
        );
        assert_eq!((dropped.unverified, dropped.outside_layer), (1, 3));
    }

    #[test]
    fn carried_items_survive_generations_unchanged_and_done_promises_leave() {
        let item = |section, quote: &str, m: &str| Item {
            section,
            quote: quote.into(),
            m: m.into(),
            why: String::new(),
        };
        let span = |items| Span {
            items,
            ..Default::default()
        };
        let first = Carry::default().merge(
            span(vec![
                item(Section::Rule, "never push main", "u1"),
                item(Section::Promise, "I will run the tests", "a1"),
            ]),
            &[],
            10,
        );
        assert_eq!(first.items[0].why, "said in this span at [m:u1]");
        let second = first.merge(
            Span {
                items: vec![
                    item(Section::Rule, "never  push main", "u9"),
                    item(Section::Term, "a lane is a worktree", "u3"),
                ],
                pointers: pointers(&[
                    ("a4".into(), "lane-commits".into()),
                    ("a5".into(), "lane-commits".into()),
                ]),
                ..Default::default()
            },
            &["I will run the tests".to_owned()],
            10,
        );
        assert_eq!(second.items.len(), 2);
        assert_eq!(
            (second.items[0].quote.as_str(), second.items[0].m.as_str()),
            ("never push main", "u1"),
            "the first wording stands"
        );
        assert!(second.items[0].why.starts_with("carried from an earlier summary"));
        assert!(second.promises().next().is_none());
        assert_eq!(second.pointers[0].hits, 2);
        let text = second.render("s");
        assert!(text.contains("## The person's rulings in force (verbatim, carried)\n\n- [m:u1] never push main"));
        assert!(text.contains("## Open promises (verbatim, carried)\n\nnone"));
        assert!(text.contains("- `lane-commits` (the work in this span touched it 2 time(s), last at [m:a5])"));
    }

    #[test]
    fn identifiers_are_read_by_shape_with_where_they_were_seen() {
        let found = ids(&[
            (
                "a1".into(),
                "committed 19e91340 on task/11058, see /work/cv-home/x.log.".into(),
            ),
            (
                "a2".into(),
                "deadbeef and 19e91340 again at https://github.com/akapug/cv/pull/3".into(),
            ),
        ]);
        let names: Vec<&str> = found.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            names,
            [
                "19e91340",
                "task/11058",
                "/work/cv-home/x.log",
                "https://github.com/akapug/cv/pull/3"
            ]
        );
        assert_eq!((found[0].first.as_str(), found[0].last.as_str()), ("a1", "a2"));
        let carry = Carry::default().merge(
            Span {
                ids: found,
                ..Default::default()
            },
            &[],
            2,
        );
        assert_eq!(carry.ids.len(), 2, "the newest kept");
        assert!(carry
            .render("s")
            .contains("probe before claiming one exists or is absent"));
    }
}
