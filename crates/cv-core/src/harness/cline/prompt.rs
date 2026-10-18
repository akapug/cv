//! Prompt metadata without retaining the prompt. Marker positions use decoded UTF-8 bytes;
//! a second bounded traversal selects the same first prose/task line as the owned codec.

use anyhow::Result;
#[derive(Debug)]
struct Selected;
impl std::fmt::Display for Selected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("prompt metadata selected")
    }
}
impl std::error::Error for Selected {}
use std::path::PathBuf;

#[derive(Default, Debug)]
pub(super) struct Summary {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub task: bool,
    pub environment: bool,
}

struct Marker {
    needle: &'static [u8],
    tail: Vec<u8>,
    found: Option<u64>,
}
impl Marker {
    fn new(needle: &'static [u8]) -> Self {
        Self {
            needle,
            tail: Vec::new(),
            found: None,
        }
    }
    fn feed(&mut self, chunk: &[u8], position: u64, after: u64) {
        if self.found.is_some() {
            return;
        }
        let mut boundary = self.tail.clone();
        boundary.extend_from_slice(&chunk[..chunk.len().min(self.needle.len() - 1)]);
        let base = position - self.tail.len() as u64;
        self.found = memchr::memmem::find_iter(&boundary, self.needle)
            .map(|i| base + i as u64)
            .find(|&i| i >= after);
        if self.found.is_none() {
            let start = after.saturating_sub(position).min(chunk.len() as u64) as usize;
            self.found = memchr::memmem::find(&chunk[start..], self.needle).map(|i| position + (start + i) as u64);
        }
        if chunk.len() >= self.needle.len() - 1 {
            self.tail.clear();
            self.tail
                .extend_from_slice(&chunk[chunk.len() - (self.needle.len() - 1)..]);
        } else {
            self.tail.extend_from_slice(chunk);
            let keep = self.needle.len() - 1;
            if self.tail.len() > keep {
                self.tail.drain(..self.tail.len() - keep);
            }
        }
    }
}

#[derive(Default)]
struct FirstLine {
    prefix: String,
    chars: u64,
    last_nonwhite: u64,
    title: Option<String>,
}
impl FirstLine {
    fn feed(&mut self, c: char) {
        if self.title.is_some() {
            return;
        }
        if c == '\n' {
            self.finish_line();
        } else if self.chars != 0 || !c.is_whitespace() {
            if self.chars < 81 {
                self.prefix.push(c);
            }
            self.chars = self.chars.saturating_add(1);
            if !c.is_whitespace() {
                self.last_nonwhite = self.chars;
                if self.last_nonwhite >= 81 {
                    self.finish_line();
                }
            }
        }
    }
    fn finish_line(&mut self) {
        if self.title.is_some() {
            return;
        }
        if self.last_nonwhite != 0 {
            let text = if self.last_nonwhite <= 81 {
                self.prefix.trim_end()
            } else {
                &self.prefix
            };
            self.title = Some(crate::ir::truncate(text, 80));
        } else {
            self.prefix.clear();
            self.chars = 0;
        }
    }
}

const CWD_PREFIX: &str = "# Current Working Directory";
#[derive(Default)]
struct Cwd {
    matched: usize,
    // 0: prefix; 1: whitespace before '('; 2: path; 3: ignore line; 4: terminal.
    state: u8,
    first: Option<u64>,
    last: u64,
    result: Option<(u64, u64)>,
}
impl Cwd {
    fn feed(&mut self, c: char, at: u64) {
        if self.state == 4 {
            return;
        }
        if c == '\n' {
            if self.state == 1 || self.state == 2 {
                // The owned extractor returns at the first prefix with malformed parentheses.
                self.state = 4;
            } else {
                self.state = 0;
                self.matched = 0;
            }
            return;
        }
        match self.state {
            0 => {
                if self.matched == 0 && c.is_whitespace() {
                    return;
                }
                if CWD_PREFIX.as_bytes().get(self.matched).copied() == Some(c as u8) && c.is_ascii() {
                    self.matched += 1;
                    if self.matched == CWD_PREFIX.len() {
                        self.state = 1;
                    }
                } else {
                    self.state = 3;
                }
            }
            1 if c.is_whitespace() => {}
            1 if c == '(' => self.state = 2,
            1 => self.state = 4,
            2 if c == ')' => {
                if let Some(first) = self.first {
                    self.result = Some((first, self.last));
                    self.state = 4;
                } else {
                    self.state = 3;
                }
            }
            2 if !c.is_whitespace() => {
                self.first.get_or_insert(at);
                self.last = at + c.len_utf8() as u64;
            }
            _ => {}
        }
    }
}

pub(super) fn summarize(mut visit: impl FnMut(&mut dyn FnMut(&str) -> Result<()>) -> Result<()>) -> Result<Summary> {
    let mut task = Marker::new(b"<task>");
    let mut task_end = Marker::new(b"</task>");
    let mut environment = Marker::new(b"<environment_details>");
    let mut environment_end = Marker::new(b"</environment_details>");
    let mut cwd = Cwd::default();
    let mut position = 0;
    visit(&mut |chunk| {
        task.feed(chunk.as_bytes(), position, 0);
        if let Some(start) = task.found {
            task_end.feed(chunk.as_bytes(), position, start + task.needle.len() as u64);
        }
        environment.feed(chunk.as_bytes(), position, 0);
        environment_end.feed(chunk.as_bytes(), position, 0);
        for (offset, c) in chunk.char_indices() {
            cwd.feed(c, position + offset as u64);
        }
        position += chunk.len() as u64;
        Ok(())
    })?;
    let task_range = task
        .found
        .zip(task_end.found)
        .map(|(start, end)| (start + task.needle.len() as u64, end));
    let environment_range = environment
        .found
        .zip(environment_end.found)
        .filter(|(start, end)| end >= start)
        .map(|(start, end)| (start, end + environment_end.needle.len() as u64));
    let mut path = String::new();
    let mut explicit = FirstLine::default();
    let mut prose = FirstLine::default();
    position = 0;
    let selected = visit(&mut |chunk| {
        for (i, c) in chunk.char_indices() {
            let at = position + i as u64;
            if cwd.result.is_some_and(|(start, end)| start <= at && at < end) {
                path.push(c);
            }
            if task_range.is_some_and(|(start, end)| start <= at && at < end) {
                explicit.feed(c);
            }
            if !environment_range.is_some_and(|(start, end)| start <= at && at < end) {
                prose.feed(c);
            }
            let next = at + c.len_utf8() as u64;
            let task_complete = task_range.is_none_or(|(_, end)| next >= end);
            if task_complete {
                explicit.finish_line();
            }
            let title_complete = explicit.title.is_some() || (task_complete && prose.title.is_some());
            let path_complete = cwd.result.is_none_or(|(_, end)| next >= end);
            if title_complete && path_complete {
                return Err(Selected.into());
            }
        }
        position += chunk.len() as u64;
        Ok(())
    });
    if let Err(error) = selected {
        if !error.is::<Selected>() {
            return Err(error);
        }
    }
    explicit.finish_line();
    prose.finish_line();
    Ok(Summary {
        title: explicit.title.or(prose.title),
        cwd: cwd.result.map(|_| PathBuf::from(path)),
        task: task.found.is_some(),
        environment: environment.found.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_prompt_cutoffs_and_early_stop_keep_identity_checks() -> Result<()> {
        let world = crate::test_world("cline-prompt-cutoffs");
        let path = world.base.join("prompt.json");
        for chars in [79, 80, 81, 82] {
            for padding in [0, 65_530, 65_536] {
                // Leading whitespace places the title across decoder windows without
                // changing the trimmed first line.
                let text = format!("{}{}  \nignored", " ".repeat(padding), "é".repeat(chars));
                for wire in [
                    serde_json::to_string(&text)?,
                    serde_json::to_string(&text)?.replace('é', "\\u00e9"),
                ] {
                    std::fs::write(&path, wire)?;
                    let source = crate::json::JsonSource::capture(&path)?;
                    let summary =
                        source.read_document(|view| summarize(|feed| view.for_each_str_chunk(feed).map(|_| ())))?;
                    let title = match chars {
                        ..=80 => "é".repeat(chars),
                        _ => "é".repeat(79) + "…",
                    };
                    assert_eq!(summary.title, Some(title));
                }
            }
        }
        let text = format!("selected title\n{}", "escaped λ ".repeat(8192));
        let wire = serde_json::to_string(&text)?.replace('λ', "\\u03bb");
        std::fs::write(&path, wire)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let mut passes = 0;
        let result = source.read_document(|view| {
            summarize(|feed| {
                passes += 1;
                let result = view.for_each_str_chunk(feed).map(|_| ());
                if passes == 2 {
                    assert!(result.as_ref().is_err_and(|error| error.is::<Selected>()));
                    std::fs::write(&path, b"\"changed\"")?;
                }
                result
            })
        });
        assert_eq!(passes, 2);
        assert!(
            result.is_err(),
            "source mutation must survive the swallowed stop marker"
        );
        Ok(())
    }

    #[test]
    fn bounded_prompt_scan_is_the_same_at_every_split() {
        // (text, title, cwd), frozen from the retired owned metadata codec.
        let late = "x".repeat(100) + "  \n<task>late title</task>";
        let wide = "x".to_string() + &" ".repeat(200) + "end";
        let wide_title = "x".to_string() + &" ".repeat(78) + "…";
        let cases = [
            ("plain\nsecond", "plain", None),
            (
                "<task>do x</task>\n<environment_details>\n# Current Working Directory (/Users/me/proj) Files\na.rs\n</environment_details>",
                "do x",
                Some("/Users/me/proj"),
            ),
            ("Just do the thing\n<environment_details>noise</environment_details>", "Just do the thing", None),
            ("雪  \r\n# Current Working Directory (/a b) Files", "雪", Some("/a b")),
            ("before\n<task>  \nexplicit é  \nsecond</task>after", "explicit é", None),
            ("pre<environment_details>hidden\nmore</environment_details>suffix", "presuffix", None),
            (
                "</environment_details>before<environment_details>hidden",
                "</environment_details>before<environment_details>hidden",
                None,
            ),
            ("<task>  </task>\nplain", "<task>  </task>", None),
            (
                "# Current Working Directory missing\n# Current Working Directory (/late)",
                "# Current Working Directory missing",
                None,
            ),
            (
                "# Current Working Directory (  )\n# Current Working Directory (/later)",
                "# Current Working Directory (  )",
                Some("/later"),
            ),
            ("<task>", "<task>", None),
            (late.as_str(), "late title", None),
            (wide.as_str(), wide_title.as_str(), None),
        ];
        for (text, title, cwd) in cases {
            for at in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
                let result = summarize(|feed| {
                    feed(&text[..at])?;
                    feed(&text[at..])?;
                    Ok(())
                })
                .unwrap();
                assert_eq!(result.title.as_deref(), Some(title), "at {at}: {text:?}");
                assert_eq!(
                    result.cwd.as_deref(),
                    cwd.map(std::path::Path::new),
                    "at {at}: {text:?}"
                );
                assert_eq!(result.task, text.contains("<task>"));
                assert_eq!(result.environment, text.contains("<environment_details>"));
            }
        }
    }
    #[test]
    fn second_pass_stops_only_after_late_title_and_path_are_settled() -> Result<()> {
        let padding = "x".repeat(8192);
        for (text, title, cwd) in [
            (format!("plain\n{padding}"), "plain", None),
            (
                format!("plain\n{padding}\n# Current Working Directory (/late-path) Files"),
                "plain",
                Some("/late-path"),
            ),
            (format!("plain\n{padding}<task>late title</task>"), "late title", None),
            (
                format!("<environment_details>{padding}</environment_details>after"),
                "after",
                None,
            ),
            (format!("<task> \n </task>\nplain\n{padding}"), "<task>", None),
        ] {
            let mut visits = 0;
            let mut bytes = [0usize; 2];
            let result = summarize(|feed| {
                let pass = visits;
                visits += 1;
                for chunk in text.as_bytes().chunks(64) {
                    // The only non-ASCII test path is deliberately kept on valid boundaries.
                    let chunk = std::str::from_utf8(chunk)?;
                    bytes[pass] += chunk.len();
                    feed(chunk)?;
                }
                Ok(())
            })?;
            assert_eq!(bytes[0], text.len());
            assert_eq!(result.title.as_deref(), Some(title));
            assert_eq!(result.cwd.as_deref(), cwd.map(std::path::Path::new));
            if text == format!("plain\n{padding}") {
                assert_eq!(bytes[1], 64);
            }
        }
        Ok(())
    }
}
