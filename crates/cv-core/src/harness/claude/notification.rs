//! Whether a `<task-notification>`'s first `<summary>` names an agent return or a workflow:
//! `tag_inner`'s first `<summary>` and its first following `</summary>`, across decoder windows,
//! with bounded state — tag and word suffixes are carried, the summary itself never copied.

/// The word a sub-agent return's summary opens with; the match counter's goal is its length.
const AGENT: &[u8] = b"Agent ";

pub(super) struct NotificationSummary {
    opened: bool,
    complete: bool,
    pending: String,
    scratch: String,
    leading: bool,
    agent_possible: bool,
    /// Bytes of [`AGENT`] matched so far; done when it reaches `AGENT.len()`.
    agent: usize,
    workflow_found: bool,
    workflow: usize,
}
impl Default for NotificationSummary {
    fn default() -> Self {
        Self {
            opened: false,
            complete: false,
            pending: String::new(),
            scratch: String::new(),
            leading: true,
            agent_possible: true,
            agent: 0,
            workflow_found: false,
            workflow: 0,
        }
    }
}
impl NotificationSummary {
    pub(super) fn push(&mut self, chunk: &str) {
        if self.complete {
            return;
        }
        self.scratch.clear();
        self.scratch.push_str(&self.pending);
        self.scratch.push_str(chunk);
        self.pending.clear();
        let mut body = self.scratch.as_str();
        if !self.opened {
            if let Some(at) = body.find("<summary>") {
                self.opened = true;
                body = &body[at + 9..];
            } else {
                let n = suffix(body.as_bytes(), b"<summary>", false);
                self.pending.push_str(&body[body.len() - n..]);
                return;
            }
        }
        if let Some(at) = body.find("</summary>") {
            body = &body[..at];
            self.complete = true;
        } else {
            let n = suffix(body.as_bytes(), b"</summary>", false);
            self.pending.push_str(&body[body.len() - n..]);
            body = &body[..body.len() - n];
        }
        if self.leading {
            body = body.trim_start();
            self.leading = body.is_empty();
        }
        if self.agent_possible && self.agent < AGENT.len() {
            let n = body.len().min(AGENT.len() - self.agent);
            if body.as_bytes()[..n] == AGENT[self.agent..self.agent + n] {
                self.agent += n;
            } else {
                self.agent_possible = false;
            }
        }
        if !self.workflow_found {
            const WORD: &[u8] = b"workflow";
            // Only a proper suffix can bridge windows. Never copy the complete summary.
            let mut bridge = [0; 14];
            bridge[..self.workflow].copy_from_slice(&WORD[..self.workflow]);
            let n = body.len().min(7);
            bridge[self.workflow..self.workflow + n].copy_from_slice(&body.as_bytes()[..n]);
            let bridge = &bridge[..self.workflow + n];
            self.workflow_found =
                contains_ascii_insensitive(bridge, WORD) || contains_ascii_insensitive(body.as_bytes(), WORD);
            self.workflow = suffix(if body.len() < 7 { bridge } else { body.as_bytes() }, WORD, true);
        }
    }

    /// The summary names a sub-agent return: a full [`AGENT`] prefix, or a workflow mention.
    pub(super) fn is_agent_return(&self) -> bool {
        self.complete && ((self.agent_possible && self.agent == AGENT.len()) || self.workflow_found)
    }
}

/// ASCII-case-insensitive substring search, allocation-free (the tests' oracle spells the same
/// check `to_ascii_lowercase().contains`).
pub(super) fn contains_ascii_insensitive(bytes: &[u8], word: &[u8]) -> bool {
    memchr::memchr2_iter(word[0], word[0].to_ascii_uppercase(), bytes).any(|at| {
        bytes
            .get(at..at + word.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(word))
    })
}

/// Longest proper suffix matching an ASCII tag/word prefix (therefore a UTF-8 boundary).
fn suffix(body: &[u8], wanted: &[u8], insensitive: bool) -> usize {
    (1..wanted.len().min(body.len() + 1))
        .rev()
        .find(|&n| {
            let end = &body[body.len() - n..];
            if insensitive {
                end.eq_ignore_ascii_case(&wanted[..n])
            } else {
                end == &wanted[..n]
            }
        })
        .unwrap_or(0)
}
