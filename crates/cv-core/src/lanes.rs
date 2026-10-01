//! The sub-agent forest as **lanes**: one row per sub-agent with what an orchestrator asks of it —
//! who it is, what it ran on, how long, how much, where it is now, and whether it is parked.
//!
//! [`crate::subagent_tree_of`] lists the forest; this module reads each transcript once (a
//! streamed pass, content inline, usage deduplicated by API `message.id` exactly as
//! `cv stats --tokens` does) and pairs it with two harness-side signals: the parent transcript's
//! `<task-notification>` records (the status Claude Code itself reported for the child) and the
//! child's own `SubagentStop` hook attachment (it stopped; a `SendMessage` resume appends turns
//! after it). The strand class — an agent that stopped with a final text saying it is *waiting*
//! for something that will never wake it — is flagged from those two facts and the text.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::harness::claude::{subagent_end, task_notices, SubagentEnd, TaskNotice};
use crate::ir::{Block, Message, MessageKind, Role, SessionRef, Usage};
use crate::stream::{Flow, ParseOptions};
use crate::SubagentInfo;

/// Token totals for one lane, following [`Usage`]'s disjoint convention.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct LaneTokens {
    pub calls: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// `input + cache_read + cache_write + output`.
    pub total: u64,
}

impl LaneTokens {
    fn add(&mut self, u: &Usage) {
        self.calls += 1;
        self.input += u.input_tokens.unwrap_or(0);
        self.cache_read += u.cache_read_tokens.unwrap_or(0);
        self.cache_write += u.cache_creation_tokens.unwrap_or(0);
        self.output += u.output_tokens.unwrap_or(0);
        self.total = self.input + self.cache_read + self.cache_write + self.output;
    }
}

/// Where a lane's `status` string was read from, so a reader knows how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    /// A `Workflow` run's `journal.jsonl` result record (`done` / `partial` / `failed` / …).
    Journal,
    /// The parent transcript's last `<task-notification>` for this agent (`completed` /
    /// `failed` / `killed` / `stopped`).
    TaskNotification,
    /// The child's own `SubagentStop` hook attachment after its last turn (`stopped`).
    SubagentStop,
    /// Nothing says it stopped: the transcript ends mid-work (`running`).
    Transcript,
}

/// One sub-agent, summarized.
#[derive(Debug, Clone, Serialize)]
pub struct Lane {
    /// The bare `agentId` (the `agent-` prefix stripped) — what `SendMessage` and the journal use.
    pub agent_id: String,
    /// The transcript's session id (`agent-<agent_id>`), as `cv show` resolves it.
    pub session_id: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// The last conversational record (`user`/`assistant`) in the transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub messages: usize,
    pub tool_calls: u64,
    pub tokens: LaneTokens,
    /// `running` · `completed` · `stopped` · `failed` · `killed`, or a journaled workflow status.
    pub status: String,
    pub status_source: StatusSource,
    /// The last assistant text turn — the return value for a finished agent, the last narration
    /// for a running one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_text: Option<String>,
    /// The last tool call (`Bash · cargo build …`): where a running or dead agent was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// Parked on a promise nothing will keep: stopped, and its last text says it is waiting.
    pub stranded: bool,
}

impl Lane {
    /// Finished for real: a terminal status AND not stranded. A stranded lane is `completed` as
    /// far as the harness knows — that is exactly the trap — so it never counts as done here.
    pub fn is_done(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "done") && !self.stranded
    }

    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
}

/// The phrases that mark a final text as a parked promise. Each is matched case-insensitively
/// against the tail of the last assistant text. They are the shapes that stranded four lanes on
/// one day (`Waiting on notifications`, `I'll continue when the monitor fires`, `waiting for the
/// … verdict`), generalized just enough to catch their siblings and no further.
pub const STRAND_PATTERNS: &[&str] = &[
    r"waiting (on|for) ([\w'’-]+ ){0,5}(notification|notifications|monitor|verdict|verdicts|build|builds|result|results|lane|lanes|run|job|report|reply|answer)\b",
    r"continue (when|once|after) (the |that |this |it )?\w*( \w+){0,2} ?(fires|lands|arrives|finishes|completes|returns|comes back|is ready|reports)\b",
    r"\b(i.?ll|i will|will) (continue|resume|pick (this|it|that) (back )?up|check back|follow up|report back|proceed) (when|once|after|as soon as)\b",
    r"\b(parked|paused|standing by|on hold) (until|for|pending)\b",
    r"\bawaiting (the |a )?(notification|notifications|verdict|result|results|monitor|build|reply|answer)\b",
    r"\b(when|once|after) the (monitor|notification|watcher|timer) fires\b",
];

fn strand_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        let joined = STRAND_PATTERNS
            .iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|");
        Regex::new(&format!("(?i){joined}")).expect("strand patterns compile")
    })
}

/// Does a final text read as a parked promise? A promise is how a parked text *ends*, so only
/// its last two sentences are consulted — a report that mentions waiting in its middle and then
/// concludes is not stranded — and past-tense narration (`was waiting on … earlier`) is removed
/// before matching, since the regex engine has no lookbehind to exclude it in place.
pub fn text_is_waiting(text: &str) -> bool {
    let sentences: Vec<&str> = text
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let start = sentences.len().saturating_sub(2);
    let tail = sentences[start..].join(". ");
    static PAST: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let past = PAST.get_or_init(|| {
        Regex::new(r"(?i)\b(was|were|had been|have been|has been) (waiting|standing by|parked)\b").unwrap()
    });
    let present = past.replace_all(&tail, "");
    strand_regex().is_match(&present)
}

/// One line for a tool call: the tool name plus the argument a reader would look at first.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let arg = pick(&[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "description",
        "prompt",
        "url",
        "skill",
    ])
    .unwrap_or_else(|| match input {
        Value::Object(m) if m.is_empty() => String::new(),
        other => other.to_string(),
    });
    let arg = crate::ir::truncate(&arg, 80);
    if arg.is_empty() {
        name.to_string()
    } else {
        format!("{name} · {arg}")
    }
}

/// What one streamed pass over a transcript yields.
#[derive(Default)]
struct Pass {
    model: Option<String>,
    messages: usize,
    tool_calls: u64,
    tokens: LaneTokens,
    last_text: Option<String>,
    last_tool: Option<String>,
}

fn pass(r: &SessionRef) -> Option<Pass> {
    let adapter = crate::harness::for_harness(r.harness)?;
    let mut p = Pass::default();
    let mut seen = std::collections::HashSet::new();
    // `extra` on so Claude's `message.id` (the usage dedupe key) is materialized; `spans` off so
    // text blocks are inline and `Message::text` is safe — content is read per message and dropped.
    let opts = ParseOptions {
        extra: true,
        ..ParseOptions::default()
    };
    let session = adapter
        .stream(r, &opts, &mut |m: Message| {
            if m.kind.is_model_visible() && matches!(m.role, Role::User | Role::Assistant) {
                p.messages += 1;
            }
            if m.role == Role::Assistant {
                if p.model.is_none() {
                    p.model = m.model.clone().filter(|s| !s.is_empty());
                }
                if let Some(u) = m.usage.as_ref().filter(|u| u.total_tokens() > 0) {
                    let dup = crate::stats::usage_key(r.harness, &m).is_some_and(|k| !seen.insert(k));
                    if !dup {
                        p.tokens.add(u);
                    }
                }
                if m.kind == MessageKind::Reply {
                    if let Some(t) = m.text().filter(|t| !t.trim().is_empty()) {
                        p.last_text = Some(t);
                    }
                }
                for b in &m.content {
                    if let Block::ToolUse { name, input, .. } = b {
                        p.tool_calls += 1;
                        p.last_tool = Some(tool_summary(name, input));
                    }
                }
            }
            Flow::Continue
        })
        .ok()?;
    if p.model.is_none() {
        p.model = session.model.clone();
    }
    Some(p)
}

/// Resolve the status of one lane from its three possible sources, most authoritative first.
fn status_of(sub: &SubagentInfo, end: &SubagentEnd, notice: Option<&TaskNotice>) -> (String, StatusSource) {
    if let Some(s) = sub.result_status.as_deref().filter(|s| !s.is_empty()) {
        return (s.to_string(), StatusSource::Journal);
    }
    // A notification that post-dates the last turn describes the current stop; an older one
    // describes a stop the agent has since been resumed from.
    if let Some(n) = notice {
        let current = match (n.ts, end.last_turn_at) {
            (Some(nt), Some(lt)) => nt >= lt,
            (Some(_), None) => true,
            (None, _) => end.stopped() == Some(true),
        };
        if current {
            return (n.status.clone(), StatusSource::TaskNotification);
        }
    }
    if end.stopped() == Some(true) {
        return ("stopped".into(), StatusSource::SubagentStop);
    }
    ("running".into(), StatusSource::Transcript)
}

fn lane_of(sub: SubagentInfo, notices: &HashMap<String, TaskNotice>) -> Option<Lane> {
    let p = pass(&sub.session)?;
    let end = subagent_end(&sub.session.path);
    let agent_id = sub.agent_id().to_string();
    let (status, status_source) = status_of(&sub, &end, notices.get(&agent_id));
    let started_at = sub.session.created_at;
    let last_turn_at = end.last_turn_at.or(sub.session.updated_at);
    let duration_ms = match (started_at, last_turn_at) {
        (Some(s), Some(e)) if e >= s => Some((e - s).num_milliseconds() as u64),
        _ => None,
    };
    // A workflow agent's journaled summary is its real return; a direct agent's is its last text.
    let last_text = sub.result_summary.clone().or(p.last_text);
    let parked = matches!(status.as_str(), "completed" | "stopped");
    let stranded = parked && last_text.as_deref().is_some_and(text_is_waiting);
    Some(Lane {
        agent_id,
        session_id: sub.session.id.clone(),
        path: sub.session.path.clone(),
        agent_type: sub.agent_type,
        description: sub.description,
        tool_use_id: sub.tool_use_id,
        workflow: sub.workflow,
        model: p.model,
        started_at,
        last_turn_at,
        duration_ms,
        messages: p.messages,
        tool_calls: p.tool_calls,
        tokens: p.tokens,
        status,
        status_source,
        last_text,
        last_tool: p.last_tool,
        stranded,
    })
}

/// Every sub-agent of `parent` as a [`Lane`], **launch order** (oldest first — the order the
/// orchestrator issued them, which is how a status table reads). Transcripts that fail to parse
/// are skipped (counted nowhere: one bad file must not hide a fleet).
pub fn lanes_of(parent: &SessionRef) -> Vec<Lane> {
    let subs = crate::subagent_tree_of(parent);
    let notices = task_notices(&parent.path);
    let mut lanes = crate::par_filter_map(subs, |s| lane_of(s, &notices));
    lanes.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.agent_id.cmp(&b.agent_id)));
    lanes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_real_strand_phrasings_match_and_a_report_does_not() {
        for t in [
            "Build is mirrored. Waiting on notifications.",
            "the build is still running; I'll continue when the monitor fires",
            "Both lanes are green. I'm now waiting for the integrator's verdict before touching main.",
            "Nothing else to do here until the hbox build finishes — standing by for the notification.",
        ] {
            assert!(text_is_waiting(t), "should read as waiting: {t:?}");
        }
        for t in [
            "sdk-ts is fixed: npm test goes from 97/106 to 110/110. The work is three commits on sdk-ts-repair.",
            "I was waiting on notifications earlier, but the build landed; the lane is complete and the branch is pushed.",
            "K-RAN works: the kernel re-executes a runner's claimed Nock run and admits the writes only when they match.",
        ] {
            assert!(!text_is_waiting(t), "should NOT read as waiting: {t:?}");
        }
    }

    #[test]
    fn only_the_tail_is_consulted() {
        let mut long = String::from("Waiting on notifications. ");
        long.push_str(&"The lane then did a great deal of work. ".repeat(40));
        long.push_str("Done; branch pushed.");
        assert!(!text_is_waiting(&long));
    }

    #[test]
    fn tool_summary_picks_the_telling_argument() {
        assert_eq!(
            tool_summary("Bash", &serde_json::json!({"command": "cargo build", "timeout": 5})),
            "Bash · cargo build"
        );
        assert_eq!(
            tool_summary("Read", &serde_json::json!({"file_path": "/x/y.rs"})),
            "Read · /x/y.rs"
        );
        assert_eq!(tool_summary("TaskStop", &serde_json::json!({})), "TaskStop");
    }
}
