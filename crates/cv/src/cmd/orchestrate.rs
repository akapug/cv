//! `cv show --kind prompt,answer` / `cv lanes` / `cv deferrals` — the orchestrator's instruments. Each answers one
//! question an orchestrating session asks about itself during a swarm: what did the person
//! actually say (and answer), where is every lane right now, and what did I promise to do later
//! that no task holds.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::{truncate, Block, Message, MessageKind, Origin, Role, SessionRef};
use cv_core::lanes::Lane;
use cv_core::sanitize::sanitize_line;
use cv_core::stream::{Flow, ParseOptions};
use regex::Regex;
use serde::Serialize;

use crate::util::{fmt_local, home_rel, parse_harness, resolve, short_id};

// ===================== cv show --kind prompt,answer =====================

/// The markers a question-dialog tool result starts with — Claude Code's two `AskUserQuestion`
/// phrasings, and Devin CLI's `User answered your questions:`. The answers a person gives through
/// that dialog never appear as a prompt — they are tool output — and an orchestrator re-reading
/// what it was told needs them in the same stream.
const ANSWER_MARKERS: &[&str] = &[
    "The user answered",
    "Your questions have been answered",
    "User answered your questions",
];

#[derive(Debug, Clone, Serialize)]
struct PromptRow {
    /// Message index in the session (what `cv show --around N` takes).
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    /// `prompt` (typed by the person) or `answer` (an `AskUserQuestion` result).
    kind: &'static str,
    text: String,
}

/// Stream `r` once and keep only what the person said: `prompt`/`human` messages and the
/// `AskUserQuestion` answers, in order, with their message indices. `window` bounds the indices
/// (`[start, end)`, `None` end = through the last) when a window or pre-compaction span is given.
fn collect_prompts(
    r: &SessionRef,
    adapter: &dyn cv_core::harness::Adapter,
    window: Option<(usize, Option<usize>)>,
    prompts: bool,
    answers: bool,
) -> Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut idx = 0usize;
    let resolver = cv_core::Resolver::new(Some(r.path.clone()));
    let mut error = None;
    adapter.try_stream(r, &ParseOptions::lazy(), &mut |m: Message| {
        let i = idx;
        idx += 1;
        if let Some((s, e)) = window {
            if i < s {
                return Flow::Continue;
            }
            if e.is_some_and(|e| i >= e) {
                return Flow::Stop;
            }
        }
        let selected = (|| -> Result<()> {
            if prompts && m.kind == MessageKind::Prompt && m.origin == Origin::Human {
                let mut text = String::new();
                for block in &m.content {
                    if let Block::Text { text: body } = block {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        body.try_for_each_chunk(&resolver, 64 * 1024, |part| {
                            text.push_str(part);
                            Ok(())
                        })?;
                    }
                }
                if !text.trim().is_empty() {
                    rows.push(PromptRow {
                        index: i,
                        timestamp: m.timestamp,
                        kind: "prompt",
                        text,
                    });
                }
            } else if answers && (m.role == Role::Tool || m.kind == MessageKind::ToolResult) {
                for block in &m.content {
                    if let Block::ToolResult { content, .. } = block {
                        if is_question_answer(content, &resolver)? {
                            rows.push(PromptRow {
                                index: i,
                                timestamp: m.timestamp,
                                kind: "answer",
                                text: content.try_resolve(&resolver)?.into_owned(),
                            });
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(failure) = selected {
            error = Some(failure);
            return Flow::Stop;
        }
        Flow::Continue
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(rows)
}

fn is_question_answer(content: &cv_core::Text, resolver: &cv_core::Resolver) -> Result<bool> {
    #[derive(Debug)]
    struct HeadComplete;
    impl std::fmt::Display for HeadComplete {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("answer marker determined")
        }
    }
    impl std::error::Error for HeadComplete {}
    let mut head = String::new();
    let mut started = false;
    let mut answer = false;
    let result = content.try_for_each_chunk(resolver, 4096, |part| {
        for c in part.chars() {
            if !started && c.is_whitespace() {
                continue;
            }
            started = true;
            head.push(c);
            if ANSWER_MARKERS.iter().any(|marker| head.starts_with(marker)) {
                answer = true;
                return Err(HeadComplete.into());
            }
            if !ANSWER_MARKERS.iter().any(|marker| marker.starts_with(&head)) {
                return Err(HeadComplete.into());
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        if !error.is::<HeadComplete>() {
            return Err(error);
        }
    }
    Ok(answer)
}

/// What the person said in `r`, within `window`: typed prompts and/or `AskUserQuestion` answers,
/// as text (each printed whole) or as one JSON array of `index, timestamp, kind, text` rows.
pub(crate) fn print_prompts(
    r: &SessionRef,
    adapter: &dyn cv_core::harness::Adapter,
    window: Option<(usize, Option<usize>)>,
    prompts: bool,
    answers: bool,
    json: bool,
) -> Result<()> {
    let rows = collect_prompts(r, adapter, window, prompts, answers)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let prompts = rows.iter().filter(|p| p.kind == "prompt").count();
    let answers = rows.len() - prompts;
    println!(
        "# prompts of {} — {prompts} typed, {answers} answered via AskUserQuestion\n",
        short_id(&r.id)
    );
    for p in &rows {
        let ts = p
            .timestamp
            .map(|t| fmt_local(t, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "----------------".into());
        let tag = if p.kind == "prompt" { "user" } else { "answer" };
        println!("[{}] {ts}  {tag}", p.index);
        // Prompts are the one thing in a transcript worth reading whole; sanitize per line so a
        // pasted control sequence can't restyle the terminal, but never truncate.
        for line in p.text.lines() {
            println!("{}", sanitize_line(line));
        }
        println!();
    }
    Ok(())
}

// ===================== cv lanes =====================

fn fmt_tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn fmt_dur(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "-".into();
    };
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// `claude-opus-4-1-20250805` → `opus-4-1`: the family and version, not the vendor or the date.
fn short_model(m: Option<&str>) -> String {
    let Some(m) = m else {
        return "-".into();
    };
    let m = m.strip_prefix("claude-").unwrap_or(m);
    // Drop a trailing -YYYYMMDD date stamp.
    let m = match m.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => m,
    };
    truncate(m, 14)
}

/// The last line of a return value: what the lane concluded, not how it began.
fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneFilter {
    All,
    Running,
    Done,
    Stranded,
    /// Not finished for real: running, stopped, killed, failed, or stranded — [`Lane::is_done`]'s
    /// complement. Across recent sessions this is the rescue list for `cv adopt`.
    Unfinished,
}

/// Which forests `cv lanes` reads.
pub(crate) enum LaneScope {
    /// One session's sub-agents.
    Session { id: String, harness: Option<String> },
    /// The sub-agents of the `n` most recently active Claude sessions whose recorded cwd is `dir`
    /// — a project's recent swarms in one table (each row carries its `session_id`).
    Recent { n: usize, dir: std::path::PathBuf },
}

fn lane_passes(l: &Lane, f: LaneFilter) -> bool {
    match f {
        LaneFilter::All => true,
        LaneFilter::Running => l.is_running(),
        LaneFilter::Done => l.is_done(),
        LaneFilter::Stranded => l.stranded,
        LaneFilter::Unfinished => !l.is_done(),
    }
}

/// Two directories are the same when their canonical paths agree; a path that no longer exists
/// (a session's recorded cwd from a deleted checkout) compares as spelled.
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// The `--tasks` lines under a lane: its endpoint, then each task it holds (short id, state,
/// title, the last note's first line), at most five.
fn print_lane_tasks(l: &Lane, tasks: &[cv_core::lanes::LaneTask]) {
    const SHOWN: usize = 5;
    let Some(endpoint) = l.endpoint.as_deref() else {
        println!(
            "          ⚑ no endpoint (no CV_ENDPOINT export in its tool calls, no lane:<name> matches its description)"
        );
        return;
    };
    let guessed = if l.endpoint_source == Some(cv_core::lanes::EndpointSource::Description) {
        " (by description)"
    } else {
        ""
    };
    if tasks.is_empty() {
        println!("          ⚑ {}{guessed}: holds no tasks", sanitize_line(endpoint));
        return;
    }
    println!(
        "          ⚑ {}{guessed}: {} task(s)",
        sanitize_line(endpoint),
        tasks.len()
    );
    for t in tasks.iter().take(SHOWN) {
        let note = t
            .last_note
            .as_deref()
            .map(|n| format!(" · {}", truncate(&sanitize_line(n), 70)))
            .unwrap_or_default();
        println!(
            "            {} [{}] {}{note}",
            t.id.get(..13).unwrap_or(&t.id),
            t.state,
            truncate(&sanitize_line(&t.title), 60)
        );
    }
    if tasks.len() > SHOWN {
        println!(
            "            (+{} more — `cv task list --assignee {}`)",
            tasks.len() - SHOWN,
            sanitize_line(endpoint)
        );
    }
}

pub(crate) fn cmd_lanes(
    scope: LaneScope,
    filter: LaneFilter,
    since: Option<String>,
    with_tasks: bool,
    json: bool,
) -> Result<()> {
    let multi = matches!(scope, LaneScope::Recent { .. });
    let (title, empty_note, sessions): (String, String, Vec<SessionRef>) = match scope {
        LaneScope::Session { id, harness } => {
            let want = parse_harness(&harness)?;
            let (r, _adapter) = resolve(&id, want)?;
            (
                format!("lanes of {}", short_id(&r.id)),
                format!("no sub-agents spawned by {}", short_id(&r.id)),
                vec![r],
            )
        }
        LaneScope::Recent { n, dir } => {
            let mut refs: Vec<SessionRef> = cv_core::sessions()
                .into_iter()
                .filter(|r| r.harness == cv_core::ir::Harness::Claude)
                .filter(|r| r.cwd.as_deref().is_some_and(|c| same_dir(c, &dir)))
                .collect();
            refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
            refs.truncate(n);
            let ids: Vec<String> = refs.iter().map(|r| short_id(&r.id)).collect();
            (
                format!(
                    "lanes of the {} most recent session(s) in {} ({})",
                    refs.len(),
                    home_rel(&dir),
                    ids.join(" ")
                ),
                format!("no Claude sessions with sub-agents in {}", home_rel(&dir)),
                refs,
            )
        }
    };
    let since = match since {
        Some(s) => {
            let d = cv_core::task::parse_duration(&s)
                .ok_or_else(|| anyhow::anyhow!("--since takes a duration: 30m, 2h, 1d, 1w (got {s:?})"))?;
            Some(
                chrono::Utc::now()
                    .checked_sub_signed(d)
                    .ok_or_else(|| anyhow::anyhow!("--since duration is outside the supported date range: {s:?}"))?,
            )
        }
        None => None,
    };
    // Which forest each lane came from, keyed by its transcript path: a `Lane` carries only its
    // own `agent-…` ref, and an adopted agent can sit in two sessions at once.
    let mut parent_of: std::collections::HashMap<std::path::PathBuf, String> = std::collections::HashMap::new();
    let mut all: Vec<Lane> = Vec::new();
    for r in &sessions {
        for l in cv_core::lanes::lanes_of(r)? {
            if since.is_none_or(|t| l.active_since(t)) {
                parent_of.insert(l.path.clone(), r.id.clone());
                all.push(l);
            }
        }
    }
    if with_tasks && all.iter().any(|lane| lane_passes(lane, filter)) {
        let outcome = cv_core::task::replay()?;
        for w in &outcome.warnings {
            eprintln!("⚠ {}", sanitize_line(w));
        }
        cv_core::lanes::attach_tasks(&mut all, &outcome.model);
    }
    let lanes: Vec<&Lane> = all.iter().filter(|lane| lane_passes(lane, filter)).collect();

    if json {
        if multi {
            // `--recent` rows carry the forest they came from as `parent_id`; one session's rows
            // keep the plain `lane` shape.
            let rows: Vec<serde_json::Value> = lanes
                .iter()
                .map(|l| {
                    let mut v = serde_json::to_value(l).unwrap_or_default();
                    v["parent_id"] = serde_json::json!(parent_of.get(&l.path));
                    v
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&lanes)?);
        }
        return Ok(());
    }
    if all.is_empty() {
        println!("{empty_note}");
        return Ok(());
    }
    let running = all.iter().filter(|l| l.is_running()).count();
    let done = all.iter().filter(|l| l.is_done()).count();
    let stranded = all.iter().filter(|l| l.stranded).count();
    let returned = all.iter().filter(|l| l.lost_notification() && !l.stranded).count();
    // The four add up to the forest: running, done, stranded, and the rest (failed / killed /
    // stopped without a waiting text / journaled partials).
    let other = all.len() - running - done - stranded;
    let mut scope = match filter {
        LaneFilter::All => String::new(),
        LaneFilter::Running => format!(" · showing {} running", lanes.len()),
        LaneFilter::Done => format!(" · showing {} done", lanes.len()),
        LaneFilter::Stranded => format!(" · showing {} stranded", lanes.len()),
        LaneFilter::Unfinished => format!(" · showing {} unfinished", lanes.len()),
    };
    if let Some(t) = since {
        scope.push_str(&format!(" · active since {}", fmt_local(t, "%m-%d %H:%M")));
    }
    let lost = if returned > 0 {
        format!(" ({returned} returned without a stop record)")
    } else {
        String::new()
    };
    // What killed the "other" lanes, when their transcripts say (the remedy differs: a
    // rate-limited lane resumes after the reset; a context death relaunches from its clone).
    let mut causes: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for l in &all {
        if let Some(c) = l.failure_cause.as_deref() {
            *causes.entry(c).or_default() += 1;
        }
    }
    let why = if causes.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            causes
                .iter()
                .map(|(c, n)| format!("{n} {c}"))
                .collect::<Vec<_>>()
                .join(" · ")
        )
    };
    println!(
        "# {title} — {} sub-agents: {running} running · {done} completed{lost} · {other} other{why} · {stranded} STRANDED{scope}\n",
        all.len(),
    );
    if lanes.is_empty() {
        println!("(none match)");
        return Ok(());
    }
    let status_of = |l: &Lane| -> String {
        if l.stranded {
            "STRANDED".to_string()
        } else if let Some(c) = &l.failure_cause {
            format!("{}:{c}", l.status)
        } else {
            truncate(&l.status, 10)
        }
    };
    let sw = lanes
        .iter()
        .map(|l| status_of(l).chars().count())
        .max()
        .unwrap_or(0)
        .max(10);
    // Across sessions every row says which forest it belongs to.
    let session_col = |l: &Lane| {
        if multi {
            format!(
                "{:<9} ",
                parent_of.get(&l.path).map(|p| short_id(p)).unwrap_or_default()
            )
        } else {
            String::new()
        }
    };
    let indent = if multi { "                    " } else { "          " };
    println!(
        "{}AGENT     {:<sw$} MODEL          STARTED         DUR  TOKENS CALLS  DESCRIPTION",
        if multi { "SESSION   " } else { "" },
        "STATUS"
    );
    for l in &lanes {
        let started = l
            .started_at
            .map(|t| fmt_local(t, "%m-%d %H:%M"))
            .unwrap_or_else(|| "-".into());
        let status = status_of(l);
        let desc = l
            .description
            .as_deref()
            .map(|d| truncate(&sanitize_line(d), 60))
            .unwrap_or_default();
        let wf = l.workflow.as_deref().map(|w| format!(" ⟐{w}")).unwrap_or_default();
        println!(
            "{}{:<9} {:<sw$} {:<14} {:<11} {:>7} {:>7} {:>5}  {}{}",
            session_col(l),
            short_id(&l.agent_id),
            status,
            short_model(l.model.as_deref()),
            started,
            fmt_dur(l.duration_ms),
            fmt_tokens(l.tokens.total),
            l.tool_calls,
            desc,
            wf,
        );
        // Running: where it is — and for how long nothing has happened (a lane quiet for hours
        // mid tool call is usually dead, killed by a restart; the harness still says nothing).
        let detail = if l.is_running() {
            let quiet = l
                .last_turn_at
                .map(|t| chrono::Utc::now().signed_duration_since(t))
                .filter(|d| d.num_minutes() >= 60)
                .map(|d| {
                    format!(
                        " · ⚠ quiet {}",
                        cv_core::task::age_short(chrono::Utc::now() - d, chrono::Utc::now())
                    )
                })
                .unwrap_or_default();
            l.last_tool.as_deref().map(|t| format!("↪ {t}{quiet}"))
        } else {
            l.last_text.as_deref().map(|t| format!("↩ {}", last_line(t)))
        };
        if let Some(d) = detail {
            println!("{indent}{}", truncate(&sanitize_line(&d), 140));
        }
        if l.stranded {
            println!("{indent}→ resume: SendMessage to {}", l.agent_id);
        }
        match l.failure_cause.as_deref() {
            Some("rate-limited") => println!(
                "          ✗ {}{} → resume with one message after the reset: SendMessage to {}",
                truncate(
                    &sanitize_line(l.failure_detail.as_deref().unwrap_or("rate-limited")),
                    80
                ),
                l.resets_at
                    .map(|t| format!(" (resets {})", fmt_local(t, "%m-%d %H:%M")))
                    .unwrap_or_default(),
                l.agent_id
            ),
            Some("context") => println!(
                "          ✗ {} → it cannot be resumed: relaunch from its clone with a STATUS hand-off",
                truncate(&sanitize_line(l.failure_detail.as_deref().unwrap_or("context")), 80)
            ),
            _ => {}
        }
        if let Some(tasks) = &l.tasks {
            print_lane_tasks(l, tasks);
        }
    }
    if stranded > 0 && filter != LaneFilter::Stranded {
        println!(
            "\n⚠ {stranded} lane(s) stopped on a promise nothing will keep — `--stranded` lists them with resume hints"
        );
    }
    if returned > 0 && filter != LaneFilter::Running {
        println!(
            "\n{returned} lane(s) show `returned`: the transcript ends in a final report and no stop was ever recorded — the harness lost the notification (a restart). Their returns are the ↩ lines; they count as done."
        );
    }
    if multi && filter == LaneFilter::Unfinished && !lanes.is_empty() {
        println!("\n→ rescue into a live session: cv adopt <agent-id>… --into <live-session>  (the transcript moves; see `cv adopt --help`)");
    }
    Ok(())
}

// ===================== cv deferrals =====================

/// The embedded phrase table: the spellings an orchestrator uses when it decides *not* to do
/// something now, each a label plus the regex that catches it (compiled case-insensitively). Every
/// match in assistant text is a deferral until a task says otherwise. This is the default only —
/// a project's own vocabulary belongs in [`phrases_file`] (one `label = regex` per line), the
/// way `prune --declassify-tokens-file` keeps term lists out of cv's source.
const DEFAULT_PHRASES: &[(&str, &str)] = &[
    (
        "later lane",
        r"\b(a |one |the )?(later|separate|follow-?up|future) lane\b",
    ),
    ("follow-up", r"\bfollow-?ups?\b"),
    ("not tonight", r"\bnot (tonight|today|now|this (pass|session|turn))\b"),
    (
        "queued",
        r"\b(queued|queue (it|this|that|them|these|those) (for|behind|after|until|up))\b",
    ),
    ("your call", r"\b(your|the (maintainer|user|owner)'?s) call\b"),
    ("decision for", r"\bdecision (for|is yours|to make)\b"),
    (
        "integrator item",
        r"\b(integrator item|for the (standing )?integrator)\b",
    ),
    ("when X lands", r"\b(when|once|after) [\w`'-]+( [\w`'-]+){0,2} lands\b"),
    ("after FINAL", r"\bafter FINAL\b"),
    ("deferred", r"\bdefer(red|ring|s)?\b"),
    (
        "later pass",
        r"\b(a |the )?(later|next|future) (pass|session|turn|day|week)\b",
    ),
    ("out of scope", r"\bout of scope\b"),
];

/// The default table's labels, in the order `cv deferrals` tries them (`cv schema` publishes them).
pub(crate) fn default_phrase_labels() -> Vec<&'static str> {
    DEFAULT_PHRASES.iter().map(|(l, _)| *l).collect()
}

/// `<config dir>/deferrals.txt`: the phrase table a project keeps outside cv's source. Used when
/// it exists and no `--phrases-file` is given.
fn phrases_file() -> Option<std::path::PathBuf> {
    cv_core::config::config_dir().map(|d| d.join("deferrals.txt"))
}

/// Parse a phrase file: one `label = regex` per line, `#` to end of line is a comment, blank lines
/// ignored. The regex is compiled case-insensitively; a line that does not compile names itself.
fn parse_phrases(body: &str, origin: &str) -> Result<Vec<(String, Regex)>> {
    let mut out = Vec::new();
    for (n, raw) in body.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((label, re)) = line.split_once('=') else {
            bail!("{origin}:{}: expected `label = regex`, got {raw:?}", n + 1);
        };
        let (label, re) = (label.trim(), re.trim());
        if label.is_empty() || re.is_empty() {
            bail!("{origin}:{}: expected `label = regex`, got {raw:?}", n + 1);
        }
        let compiled = Regex::new(&format!("(?i){re}")).with_context(|| format!("{origin}:{}: regex {re:?}", n + 1))?;
        out.push((label.to_string(), compiled));
    }
    if out.is_empty() {
        bail!("{origin}: no phrases (one `label = regex` per line)");
    }
    Ok(out)
}

/// The phrase table in force: `--phrases-file`, else the config dir's `deferrals.txt` if it exists,
/// else the embedded default.
fn load_phrases(file: Option<&std::path::Path>) -> Result<Vec<(String, Regex)>> {
    let path = match file {
        Some(p) => Some(p.to_path_buf()),
        None => phrases_file().filter(|p| p.is_file()),
    };
    match path {
        Some(p) => {
            let body =
                std::fs::read_to_string(&p).with_context(|| format!("reading --phrases-file {}", p.display()))?;
            parse_phrases(&body, &p.display().to_string())
        }
        None => Ok(DEFAULT_PHRASES
            .iter()
            .map(|(label, re)| {
                (
                    label.to_string(),
                    Regex::new(&format!("(?i){re}")).expect("default deferral phrase compiles"),
                )
            })
            .collect()),
    }
}

#[derive(Debug, Clone, Serialize)]
struct Deferral {
    /// Message index in the session.
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    /// Which phrase fired (its label).
    phrase: String,
    /// ~100 chars centred on the match, newlines flattened.
    context: String,
    /// The sentence the match sits in — what the task cross-reference is scored against.
    #[serde(skip)]
    sentence: String,
    /// `--open-tasks` only: the best-matching task, if any task shares enough words.
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<TaskMatch>,
}

#[derive(Debug, Clone, Serialize)]
struct TaskMatch {
    task_id: String,
    title: String,
    state: String,
    /// The significant words the deferral and the task share.
    shared: Vec<String>,
}

/// The sentence of `text` containing byte range `at` (split on `.`/`!`/`?`/newline), capped.
fn sentence_around(text: &str, at: (usize, usize)) -> String {
    let bytes = text.as_bytes();
    let is_break = |b: u8| matches!(b, b'.' | b'!' | b'?' | b'\n');
    let mut s = at.0;
    while s > 0 && !is_break(bytes[s - 1]) {
        s -= 1;
    }
    let mut e = at.1;
    while e < bytes.len() && !is_break(bytes[e]) {
        e += 1;
    }
    // Snap to char boundaries (the breaks are ASCII, so only the walk-back can land inside a char).
    while !text.is_char_boundary(s) {
        s -= 1;
    }
    while !text.is_char_boundary(e) {
        e += 1;
    }
    truncate(text[s..e].trim(), 300)
}

/// 100 chars centred on the match, newlines flattened, ellipses where it was cut.
fn context_around(text: &str, at: (usize, usize)) -> String {
    let half = 50usize;
    let mut s = at.0.saturating_sub(half);
    let mut e = (at.1 + half).min(text.len());
    while !text.is_char_boundary(s) {
        s -= 1;
    }
    while !text.is_char_boundary(e) {
        e += 1;
    }
    let mut out = String::new();
    if s > 0 {
        out.push('…');
    }
    out.push_str(&text[s..e].replace('\n', " "));
    if e < text.len() {
        out.push('…');
    }
    out
}

const STOPWORDS: &[&str] = &[
    "that",
    "this",
    "with",
    "from",
    "have",
    "will",
    "when",
    "lands",
    "after",
    "into",
    "then",
    "than",
    "they",
    "them",
    "there",
    "their",
    "what",
    "which",
    "while",
    "would",
    "could",
    "should",
    "about",
    "been",
    "were",
    "also",
    "just",
    "only",
    "more",
    "some",
    "such",
    "very",
    "each",
    "other",
    "over",
    "under",
    "those",
    "these",
    "where",
    "here",
    "lane",
    "lanes",
    "later",
    "follow",
    "followup",
    "queued",
    "queue",
    "call",
    "decision",
    "integrator",
    "item",
    "tonight",
    "final",
    "defer",
    "deferred",
    "pass",
    "session",
    "once",
    "before",
    "because",
    "does",
    "done",
    "doing",
    "make",
    "makes",
    "need",
    "needs",
    "still",
    "until",
    "through",
    "being",
    "want",
    "wants",
    "like",
    "same",
    "both",
    "much",
    "many",
    "most",
    "next",
    "first",
    "last",
    "work",
    "task",
    "tasks",
    "today",
    "already",
    "scope",
];

/// The words a cross-reference is scored on: lowercase, ≥ 4 bytes, alphanumeric, not a stopword.
fn significant_words(text: &str) -> HashSet<String> {
    significant_words_in([text])
}

fn significant_words_in<'a>(texts: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    texts
        .into_iter()
        .flat_map(|text| text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_'))
        .map(|w| w.trim_matches(|c| c == '-' || c == '_').to_ascii_lowercase())
        .filter(|w| w.len() >= 4 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// One deferral operation's task vocabulary. Borrow projections; title/body/issue are separated
/// by whitespace in the original haystack, so their token sets can be united without copying it.
struct TaskMatcher<'a> {
    tasks: Vec<(&'a cv_core::task::TaskProjection, HashSet<String>)>,
}
impl<'a> TaskMatcher<'a> {
    fn new(model: &'a cv_core::task::TaskReadModel) -> Self {
        Self {
            tasks: model
                .tasks
                .values()
                .map(|task| {
                    let words = significant_words_in([
                        task.title.as_str(),
                        task.body.as_str(),
                        task.issue.as_deref().unwrap_or(""),
                    ]);
                    (task, words)
                })
                .collect(),
        }
    }

    /// The task sharing the most significant words, if that is at least three. Ties go to the
    /// newer task id; allocate and sort output words only after the winner has been selected.
    fn best_task(&self, sentence: &str) -> Option<TaskMatch> {
        let words = significant_words(sentence);
        if words.len() < 3 {
            return None;
        }
        let mut best: Option<(usize, &cv_core::task::TaskProjection, &HashSet<String>)> = None;
        for (task, theirs) in &self.tasks {
            let shared = words.intersection(theirs).count();
            if shared < 3 {
                continue;
            }
            let better = match &best {
                None => true,
                Some((n, prev, _)) => shared > *n || (shared == *n && task.task_id > prev.task_id),
            };
            if better {
                best = Some((shared, *task, theirs));
            }
        }
        best.map(|(_, task, theirs)| {
            let mut shared: Vec<String> = words.intersection(theirs).cloned().collect();
            shared.sort();
            TaskMatch {
                task_id: task.task_id.clone(),
                title: task.title.clone(),
                state: cv_core::task::effective_display(task),
                shared,
            }
        })
    }
}

fn collect_deferrals(
    r: &SessionRef,
    adapter: &dyn cv_core::harness::Adapter,
    since: usize,
    regexes: &[(String, Regex)],
) -> Result<Vec<Deferral>> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    adapter.try_stream(r, &ParseOptions::default(), &mut |m: Message| {
        let i = idx;
        idx += 1;
        if i < since || m.role != Role::Assistant || m.kind != MessageKind::Reply || m.origin != Origin::Model {
            return Flow::Continue;
        }
        let Some(text) = m.text() else {
            return Flow::Continue;
        };
        // One hit per phrase per message: the same promise repeated in one reply is one deferral.
        for (label, re) in regexes {
            if let Some(mm) = re.find(&text) {
                let at = (mm.start(), mm.end());
                out.push(Deferral {
                    index: i,
                    timestamp: m.timestamp,
                    phrase: label.clone(),
                    context: context_around(&text, at),
                    sentence: sentence_around(&text, at),
                    matched: None,
                });
            }
        }
        Flow::Continue
    })?;
    Ok(out)
}

/// Exit status 1 when `--open-tasks` finds an unmatched deferral, so a closeout can gate on it.
pub(crate) fn cmd_deferrals(
    id: &str,
    harness: Option<String>,
    since: Option<usize>,
    open_tasks: bool,
    phrases_file: Option<std::path::PathBuf>,
    json: bool,
) -> Result<bool> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let regexes = load_phrases(phrases_file.as_deref())?;
    let mut found = collect_deferrals(&r, adapter.as_ref(), since.unwrap_or(0), &regexes)?;

    if open_tasks {
        let outcome = cv_core::task::replay()?;
        for w in &outcome.warnings {
            eprintln!("⚠ task log: {w}");
        }
        if !found.is_empty() {
            let matcher = TaskMatcher::new(&outcome.model);
            for d in &mut found {
                d.matched = matcher.best_task(&d.sentence);
            }
        }
    }
    let unmatched = if open_tasks {
        found.iter().filter(|d| d.matched.is_none()).count()
    } else {
        0
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(unmatched == 0);
    }

    let tally = if open_tasks {
        format!(" ({} matched, {unmatched} UNMATCHED)", found.len() - unmatched)
    } else {
        String::new()
    };
    println!(
        "# deferrals in {} — {} found{tally}{}\n",
        short_id(&r.id),
        found.len(),
        since.map(|n| format!(" · from msg {n}")).unwrap_or_default()
    );
    if found.is_empty() {
        println!("(none)");
        return Ok(true);
    }
    // Ids are rendered at a collision-free length: tasks opened in one batch share 8 hex digits.
    let plen = cv_core::task::unique_prefix_len(
        found
            .iter()
            .filter_map(|d| d.matched.as_ref())
            .map(|m| m.task_id.as_str()),
        8,
    );
    for d in &found {
        let ts = d
            .timestamp
            .map(|t| fmt_local(t, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "----------------".into());
        println!(
            "[{}] {ts}  {:<16} {}",
            d.index,
            format!("\"{}\"", d.phrase),
            sanitize_line(&d.context)
        );
        if open_tasks {
            match &d.matched {
                Some(m) => println!(
                    "       MATCHED   {} [{}] {}  (shared: {})",
                    &m.task_id[..plen.min(m.task_id.len())],
                    m.state,
                    truncate(&sanitize_line(&m.title), 70),
                    m.shared.join(", ")
                ),
                None => println!("       UNMATCHED"),
            }
        }
    }
    if open_tasks && unmatched > 0 {
        println!("\n✗ {unmatched} deferral(s) have no task — `cv task open` each, or say why not");
    }
    Ok(unmatched == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_selectors_only_resolve_selected_bodies_and_keep_window_indices() -> Result<()> {
        struct Fixture(Vec<Message>);
        impl cv_core::Adapter for Fixture {
            fn harness(&self) -> cv_core::Harness {
                cv_core::Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &ParseOptions,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<cv_core::Session> {
                assert!(opts.spans);
                for m in &self.0 {
                    if sink.try_message(m.clone())? == Flow::Stop {
                        break;
                    }
                }
                Ok(cv_core::Session::new(&r.id, r.harness))
            }
        }
        let world = crate::test_world("prompt-selected-demands");
        let prompt_path = world.base.join("prompt.txt");
        let answer_path = world.base.join("answer.txt");
        let prompt = "typed🌙\n".repeat(1000);
        let answer = format!("{}The user answered with a complete answer", "\u{2003}".repeat(3000));
        std::fs::write(&prompt_path, &prompt)?;
        std::fs::write(&answer_path, &answer)?;
        let span = |path: &std::path::Path, len: usize| {
            cv_core::Text::Span(cv_core::Span {
                source: Some(path.into()),
                offset: 0,
                len: len as u64,
                escaped: false,
            })
        };
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: span(&prompt_path, prompt.len()),
        });
        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "question".into(),
            content: span(&answer_path, answer.len()),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        let mut irrelevant = Message::new(Role::Assistant);
        irrelevant.content.push(Block::Text {
            text: span(&world.base.join("not-demanded"), 8000000),
        });
        let fixture = Fixture(vec![irrelevant, user, tool]);
        let r = SessionRef::new("selection", cv_core::Harness::Claude, world.base.join("source"));
        let both = collect_prompts(&r, &fixture, None, true, true)?;
        assert_eq!(both.len(), 2);
        assert_eq!((both[0].index, both[0].text.as_str()), (1, prompt.as_str()));
        assert_eq!((both[1].index, both[1].text.as_str()), (2, answer.as_str()));
        let window = collect_prompts(&r, &fixture, Some((2, Some(3))), true, true)?;
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].index, 2);
        std::fs::remove_file(&answer_path)?;
        assert_eq!(collect_prompts(&r, &fixture, None, true, false)?[0].text, prompt);
        assert!(collect_prompts(&r, &fixture, None, false, true).is_err());
        Ok(())
    }

    #[test]
    fn answer_marker_projection_matches_trim_start_at_partial_and_unicode_boundaries() -> Result<()> {
        for text in [
            "",
            "The user",
            "The user answered",
            "Your questions have been answered:\nreply",
            "\u{2003}\tUser answered your questions:reply",
            "ordinary output",
            "The user answeréd",
        ] {
            assert_eq!(
                is_question_answer(&text.into(), &cv_core::Resolver::new(None))?,
                ANSWER_MARKERS
                    .iter()
                    .any(|marker| text.trim_start().starts_with(marker))
            );
        }
        Ok(())
    }

    fn best_task_oracle(model: &cv_core::task::TaskReadModel, sentence: &str) -> Option<TaskMatch> {
        let words = significant_words(sentence);
        if words.len() < 3 {
            return None;
        }
        let mut best: Option<(usize, &cv_core::task::TaskProjection, Vec<String>)> = None;
        for t in model.tasks.values() {
            let hay = format!("{} {} {}", t.title, t.body, t.issue.as_deref().unwrap_or(""));
            let theirs = significant_words(&hay);
            let mut shared: Vec<String> = words.intersection(&theirs).cloned().collect();
            if shared.len() < 3 {
                continue;
            }
            shared.sort();
            // Ties go to the newer task (ids are time-sortable), which is the one most likely to be
            // the deferral's own.
            let better = match &best {
                None => true,
                Some((n, prev, _)) => shared.len() > *n || (shared.len() == *n && t.task_id > prev.task_id),
            };
            if better {
                best = Some((shared.len(), t, shared));
            }
        }
        best.map(|(_, t, shared)| TaskMatch {
            task_id: t.task_id.clone(),
            title: t.title.clone(),
            state: cv_core::task::effective_display(t),
            shared,
        })
    }

    #[test]
    fn deferral_matcher_preserves_scores_ties_states_and_word_normalization() {
        use cv_core::task::{TaskEventKind, TaskReducer};
        let specs = [
            ("ALPHA", "bravo", Some("charlie")),
            ("alpha bravo", "charlie delta", None),
            ("alpha", "bravo charlie", Some("irrelevant")),
            (
                "external_kind",
                "ümlaut resource-admission",
                Some("ÉCLAIR words_boundary"),
            ),
            ("foo_", "_bar baz-", Some("-quux")),
        ];
        let events: Vec<_> = specs
            .iter()
            .enumerate()
            .map(|(index, (title, body, issue))| {
                let mut event = cv_core::task::new_event(
                    None,
                    "agent:author",
                    TaskEventKind::Opened {
                        title: (*title).into(),
                        body: (*body).into(),
                        repo: None,
                        issue: issue.map(String::from),
                        channel: None,
                        assignee: None,
                    },
                );
                event.id = format!("01800000-0000-7000-8000-{:012x}", index + 1);
                event.task_id = event.id.clone();
                event
            })
            .collect();
        let mut model = TaskReducer::reduce(&events).unwrap();
        // Deferrals match terminal tasks too; retain that behavior despite --open-tasks spelling.
        model.tasks.get_mut(&events[2].task_id).unwrap().state = cv_core::task::TaskState::Done;
        let matcher = TaskMatcher::new(&model);
        let sentences = [
            "",
            "the next task is deferred until later",
            "alpha bravo",
            "alpha alpha bravo bravo",
            "ALPHA, bravo; charlie.",
            "alpha bravo charlie delta",
            "alpha bravo charlie delta irrelevant",
            "external_kind ümlaut resource-admission ÉCLAIR words_boundary",
            "EXTERNAL_KIND ümlaut resource-admission",
            "foo bar baz quux",
            "alpha bravo charlie words_boundary unmatched",
            "-alpha- _bravo_ charlie",
        ];
        for _ in 0..2 {
            for sentence in sentences {
                assert_eq!(
                    serde_json::to_value(matcher.best_task(sentence)).unwrap(),
                    serde_json::to_value(best_task_oracle(&model, sentence)).unwrap(),
                    "{sentence}"
                );
            }
        }
        let tied = matcher.best_task("alpha bravo charlie").unwrap();
        assert_eq!(tied.task_id, events[2].task_id);
        assert_eq!(tied.state, "done");
        assert_eq!(tied.shared, ["alpha", "bravo", "charlie"]);
        assert_eq!(
            matcher.best_task("alpha bravo charlie delta").unwrap().task_id,
            events[1].task_id
        );
        assert!(TaskMatcher::new(&cv_core::task::TaskReadModel::default())
            .best_task("alpha bravo charlie")
            .is_none());
    }

    #[test]
    fn phrases_compile_and_catch_the_real_sentences() {
        let regexes = load_default();
        let hits = |s: &str| -> Vec<&str> {
            regexes
                .iter()
                .filter(|(_, re)| re.is_match(s))
                .map(|(l, _)| l.as_str())
                .collect()
        };
        assert!(hits("an inconsistency to settle in one later lane, not tonight.").contains(&"later lane"));
        assert!(hits("an inconsistency to settle in one later lane, not tonight.").contains(&"not tonight"));
        assert!(hits("is queued for the standing integrator right after FINAL-1 lands").contains(&"queued"));
        assert!(hits("I'll queue it for the integration step.").contains(&"queued"));
        assert!(!hits("a queue is not an architecture").contains(&"queued"));
        assert!(hits("is queued for the standing integrator right after FINAL-1 lands").contains(&"after FINAL"));
        assert!(hits("is queued for the standing integrator right after FINAL-1 lands").contains(&"when X lands"));
        assert!(hits("that one is your call").contains(&"your call"));
        assert!(hits("the maintainer's call, not mine").contains(&"your call"));
        assert!(hits("I'll defer the IPA leg to a later pass").contains(&"deferred"));
        assert!(hits("I'll defer the IPA leg to a later pass").contains(&"later pass"));
        assert!(
            hits("the tests pass and the build is green").is_empty(),
            "{:?}",
            hits("the tests pass and the build is green")
        );
    }

    /// The embedded table, compiled — what `load_phrases(None)` yields when no config file exists.
    fn load_default() -> Vec<(String, Regex)> {
        DEFAULT_PHRASES
            .iter()
            .map(|(l, re)| (l.to_string(), Regex::new(&format!("(?i){re}")).unwrap()))
            .collect()
    }

    #[test]
    fn phrase_files_parse_in_order_and_name_their_errors() {
        let got = parse_phrases(
            "# a project's own vocabulary\nparking lot = \\bparking lot\\b   # trailing note\n\nnext sprint = \\bnext sprint\\b\n",
            "t",
        )
        .unwrap();
        assert_eq!(
            got.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            ["parking lot", "next sprint"]
        );
        assert!(got[0].1.is_match("into the PARKING LOT it goes"), "case-insensitive");
        let err = parse_phrases("no equals sign here\n", "t").unwrap_err().to_string();
        assert!(err.contains("t:1") && err.contains("label = regex"), "{err}");
        let err = parse_phrases("broken = \\b(unclosed\n", "t").unwrap_err().to_string();
        assert!(err.contains("t:1"), "{err}");
        assert!(parse_phrases("# only comments\n", "t").is_err());
        // A named file that does not exist is an error naming the path, never a silent default.
        let err = load_phrases(Some(std::path::Path::new("/nonexistent/deferrals.txt")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("phrases-file"), "{err}");
    }

    #[test]
    fn context_and_sentence_are_bounded_and_char_safe() {
        let text = "Ünïcode before. The Host umbrella — is queued for the standing integrator right after FINAL-1 lands, since it touches the file. Next sentence.";
        let at = text.find("queued").map(|s| (s, s + 6)).unwrap();
        let ctx = context_around(text, at);
        assert!(ctx.chars().count() <= 104, "{ctx}");
        assert!(ctx.contains("queued"));
        let sent = sentence_around(text, at);
        assert!(sent.starts_with("The Host umbrella"), "{sent}");
        assert!(sent.ends_with("touches the file"), "{sent}");
    }

    #[test]
    fn significant_words_drop_stopwords_and_short_tokens() {
        let w = significant_words("Make ResourceTargetAdmission.externalKind EXHAUSTIVE after FINAL-1 lands");
        assert!(w.contains("resourcetargetadmission"));
        assert!(w.contains("externalkind"));
        assert!(w.contains("exhaustive"));
        assert!(!w.contains("after"));
        assert!(w.contains("final-1"));
        assert!(!w.contains("make"));
    }
}
