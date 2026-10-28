//! `cv precompact`: write a live session's compaction summary before its harness asks for it,
//! keep it current as the session grows, and answer the harness's compaction request from it.
//!
//! The core (record, anchors, validation, profiles, prompts) is `cv_core::precompact`; this module
//! is the thin driver around it: the resident follower (`watch`), one-shot writes (`now`), the
//! lookup a proxy calls once per compaction (`answer`), the replay check (`check`), the no-proxy
//! fallback (`swap`), and the measurements (`report`). Design and measurements:
//! `docs/design/PRECOMPACT.md`.

use crate::util::{fmt_int, parse_harness, short_id, usage, Wait};
use anyhow::{Context, Result};
use clap::Subcommand;
use cv_core::ir::{Block, Harness, Message, MessageKind, Role, SessionRef};
use cv_core::precompact::{self as pc, prompt, prompt::Turn, store, Record, Settings, Window};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The answer-time update's output cap: a short section, not a second summary.
const TAIL_MAX_TOKENS: u32 = 4096;
/// A summary's output cap. The harness's own summaries on our fleet ran 9-17k tokens.
const SUMMARY_MAX_TOKENS: u32 = 20_000;
/// How long one summary call may take before it is abandoned (the session keeps its old record).
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(900);
/// How long the answer-time pass may take before the answer falls through to the harness's own
/// compaction (exit 3): the stall it is meant to shorten.
const TAIL_TIMEOUT: Duration = Duration::from_secs(120);
/// The first retry of a session whose read failed (it changed while being read); each further
/// failure doubles it, up to [`RETRY_MAX`]. Cost-only: a session written to faster than it can
/// be read whole (a multi-GB transcript of an active seat) costs one whole read per interval.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(600);
/// Exit code for a miss: the caller falls through to the harness's own compaction.
pub(crate) const MISS_EXIT: i32 = 3;

#[derive(Subcommand)]
pub(crate) enum PrecompactCmd {
    /// Follow live sessions and keep each one's summary current: start at a fraction of the
    /// harness's auto-compact point, extend by new tokens or when the session goes idle. Wakes on
    /// file changes (inotify on Linux), not a timer. Sessions whose harness precomputes its own
    /// compactions are left alone.
    Watch {
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Decide and log, but call no model and write no record.
        #[arg(long)]
        dry_run: bool,
        /// Follow for this long (30s, 5m, 2h), then exit 0.
        #[arg(id = "for", long = "for", value_name = "DURATION")]
        for_: Option<String>,
        #[command(flatten)]
        knobs: Knobs,
    },
    /// Write (or extend) one session's summary now, up to its newest message.
    Now {
        /// Session id, `harness:id`, or a transcript path.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Print what would be sent and written, call nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        knobs: Knobs,
    },
    /// Answer a compaction request: the request JSON (or an envelope `{body, headers, harness,
    /// session, instruction}`) on stdin; the summary on stdout. A miss exits 3 with the reason on
    /// stderr, and the caller sends the request on as it came.
    Answer {
        /// The harness that sent the request (default: found from the stored record).
        #[arg(long)]
        harness: Option<String>,
        /// Serve only when nothing follows the cut; a tail is a miss instead of a model call.
        #[arg(long)]
        no_tail_pass: bool,
        /// The model for the tail pass (default: `precompact.model`, else the provider's).
        #[arg(long)]
        model: Option<String>,
    },
    /// Check a captured compaction request against a session's fingerprint, with no store and no
    /// model: the replay behind the anchor match rate. Prints one JSON verdict.
    Check {
        /// The captured request (JSON: a body, or `{body, headers}`).
        request: PathBuf,
        /// The session: id, `harness:id`, or a transcript path.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Read only the transcript's first N bytes (the state when the request was sent).
        #[arg(long, value_name = "BYTES")]
        upto_bytes: Option<u64>,
        /// Cut where the window's fill last stood at or under this many tokens (a precompute
        /// made earlier); default: at the newest message.
        #[arg(long, value_name = "TOKENS")]
        cut_tokens: Option<u64>,
    },
    /// Harness compaction instructions captured from real requests.
    Profile {
        #[command(subcommand)]
        action: ProfileCmd,
    },
    /// The stored summaries.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// What the measurements say: compaction stalls by method, answers served and missed.
    Report {
        #[arg(long)]
        json: bool,
    },
    /// The fallback without a proxy (Claude Code): a NEW session holding the stored summary as
    /// its compaction and the messages after the cut, to resume into. The source is untouched.
    Swap {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// Capture the compaction instruction of a request file.
    Import {
        request: PathBuf,
        /// The harness that sent it.
        #[arg(long)]
        harness: String,
    },
    /// List the captured profiles.
    Ls,
}

/// The threshold flags; each overrides `[precompact]` in the config, which overrides the named
/// defaults in `cv_core::precompact`.
#[derive(clap::Args, Default)]
pub(crate) struct Knobs {
    /// The harness's auto-compact point in tokens (default: learned from its own records).
    #[arg(long, value_name = "TOKENS")]
    compact_at: Option<u64>,
    /// Start at this fraction of the auto-compact point (default 0.70).
    #[arg(long, value_name = "F")]
    start_fraction: Option<f64>,
    /// Extend after this many new tokens (default 20000).
    #[arg(long, value_name = "TOKENS")]
    extend_tokens: Option<u64>,
    /// Extend when the session has been idle this long (default 45).
    #[arg(long, value_name = "SECS")]
    idle_secs: Option<u64>,
    /// Bytes of each tool result the summary model reads (default 2048).
    #[arg(long, value_name = "BYTES")]
    tool_result_bytes: Option<usize>,
    /// The summary model.
    #[arg(long)]
    model: Option<String>,
    /// Summary jobs at once (default 2).
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,
    /// Follow sessions touched within this many seconds of startup (default 7200).
    #[arg(long, value_name = "SECS")]
    live_within_secs: Option<u64>,
}

impl Knobs {
    fn settings(&self) -> Settings {
        let mut s = cv_core::config::load().precompact.unwrap_or_default();
        if self.compact_at.is_some() {
            s.compact_at = self.compact_at;
        }
        macro_rules! take {
            ($($f:ident),*) => {$(if let Some(v) = self.$f.clone() { s.$f = v; })*};
        }
        take!(
            start_fraction,
            extend_tokens,
            idle_secs,
            tool_result_bytes,
            jobs,
            live_within_secs
        );
        if self.model.is_some() {
            s.model = self.model.clone();
        }
        s.jobs = s.jobs.max(1);
        s
    }
}

pub(crate) fn cmd_precompact(action: PrecompactCmd) -> Result<()> {
    match action {
        PrecompactCmd::Watch {
            harness,
            cwd,
            dry_run,
            for_,
            knobs,
        } => watch(
            parse_harness(&harness)?,
            cwd,
            dry_run,
            for_.as_deref(),
            knobs.settings(),
        ),
        PrecompactCmd::Now {
            id,
            harness,
            dry_run,
            knobs,
        } => now(&id, parse_harness(&harness)?, dry_run, &knobs.settings()),
        PrecompactCmd::Answer {
            harness,
            no_tail_pass,
            model,
        } => answer(harness, no_tail_pass, model),
        PrecompactCmd::Check {
            request,
            id,
            harness,
            upto_bytes,
            cut_tokens,
        } => check(&request, &id, parse_harness(&harness)?, upto_bytes, cut_tokens),
        PrecompactCmd::Profile { action } => profile(action),
        PrecompactCmd::Ls { json } => ls(json),
        PrecompactCmd::Report { json } => report(json),
        PrecompactCmd::Swap { id, harness, dry_run } => swap(&id, parse_harness(&harness)?, dry_run),
    }
}

// ───────────────────────────── reading a session ─────────────────────────────

fn text_of(t: &cv_core::Text, resolver: &cv_core::Resolver) -> Result<String> {
    let mut s = String::new();
    t.try_for_each_chunk(resolver, 64 * 1024, |part| {
        s.push_str(part);
        Ok(())
    })?;
    Ok(s)
}

/// One message as the summary model reads it: prose whole, tool calls and results cut to
/// `tool_bytes`, reasoning left out. `None` for what no window carries (boundaries, notices).
fn render(m: &Message, resolver: &cv_core::Resolver, tool_bytes: usize) -> Result<Option<Turn>> {
    if matches!(
        m.kind,
        MessageKind::CompactionBoundary | MessageKind::SystemPrompt | MessageKind::Notice | MessageKind::ModelChange
    ) {
        return Ok(None);
    }
    let role = if m.role == Role::Assistant { "assistant" } else { "user" };
    let injected = m.kind == MessageKind::InjectedContext;
    let mut out = String::new();
    for b in &m.content {
        let piece = match b {
            Block::Text { text } => {
                let t = text_of(text, resolver)?;
                if injected {
                    prompt::clip(&t, tool_bytes).into_owned()
                } else {
                    t
                }
            }
            Block::Thinking { .. } => continue,
            Block::ToolUse { name, input, .. } => {
                let mut args = String::new();
                cv_core::projection::append_json(&mut args, input)?;
                format!("[tool call: {name} {}]", prompt::clip(&args, tool_bytes))
            }
            Block::ToolResult { content, .. } => {
                format!(
                    "[tool result] {}",
                    prompt::clip(&text_of(content, resolver)?, tool_bytes)
                )
            }
            Block::Image { .. } => "[image]".into(),
            Block::File { path, .. } => format!("[file {}]", path.as_deref().unwrap_or("")),
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&piece);
    }
    let out = out.trim();
    Ok((!out.is_empty()).then(|| Turn {
        role,
        content: out.to_owned(),
    }))
}

/// A session being followed: its window, the rendered span since the record's cut, its record.
struct Live {
    r: SessionRef,
    window: Window,
    span: Vec<(usize, Turn)>,
    record: Option<Record>,
    job: bool,
    primed: bool,
    said: &'static str,
    /// Messages delivered by the watcher, and how many of them a completed read accepted. A read
    /// that fails after delivering some is re-delivered from the accepted position.
    delivered: usize,
    accepted: usize,
    /// Messages of a failed read still to come again, already observed: skipped.
    redelivered: usize,
}

impl Live {
    fn new(r: SessionRef) -> Live {
        Live {
            r,
            window: Window::default(),
            span: Vec::new(),
            record: None,
            job: false,
            primed: false,
            said: "",
            delivered: 0,
            accepted: 0,
            redelivered: 0,
        }
    }

    /// A message the watcher delivered: whether it is new (`false`: a failed read's message
    /// delivered again, already observed).
    fn take(&mut self) -> bool {
        self.delivered += 1;
        if self.redelivered > 0 {
            self.redelivered -= 1;
            return false;
        }
        true
    }

    /// The watcher reads the session from its start again: everything observed comes again.
    fn restart(&mut self) {
        self.redelivered += self.delivered;
        self.delivered = 0;
        self.accepted = 0;
    }

    /// The watcher's read of this session ended: accepted through what was delivered, or failed,
    /// so it comes again from the last accepted position.
    fn settle(&mut self, accepted: bool) {
        if accepted {
            self.accepted = self.delivered;
        } else {
            self.redelivered += self.delivered - self.accepted;
            self.delivered = self.accepted;
        }
    }

    fn key(&self) -> String {
        format!("{}:{}", self.r.harness.as_str(), self.r.id)
    }

    /// Take the next message. A compaction the harness did ends the window and its record, and
    /// once primed is logged with what cv held for it.
    fn observe(&mut self, m: &Message, settings: &Settings, root: &Path, dry_run: bool) -> Result<()> {
        let resolver = cv_core::Resolver::new(Some(self.r.path.clone()));
        let before = self.window.boundaries.len();
        let index = self.window.messages;
        self.window.observe(m, &resolver)?;
        if self.window.boundaries.len() > before {
            self.span.clear();
            let b = self.window.boundaries.last().expect("a boundary was just pushed");
            let record = self.record.take();
            if self.primed {
                let _ = store::event(
                    root,
                    json!({
                        "event": "compaction",
                        "harness": self.r.harness.as_str(),
                        "session": self.r.id,
                        "trigger": b.trigger,
                        "pre_tokens": b.pre_tokens,
                        "duration_ms": b.duration_ms,
                        "native": b.precomputed,
                        "had_record": record.is_some(),
                        "record_cut_tokens": record.as_ref().and_then(|r| r.cut.tokens),
                    }),
                );
                eprintln!(
                    "cv precompact: {} compacted ({}, {} ms{}{})",
                    short_id(&self.r.id),
                    b.trigger.as_deref().unwrap_or("?"),
                    b.duration_ms.map_or("?".into(), fmt_int),
                    if b.precomputed { ", native precompute" } else { "" },
                    if record.is_some() { ", cv held a summary" } else { "" },
                );
            }
            if record.is_some() && !dry_run {
                let _ = store::remove_record(root, self.r.harness.as_str(), &self.r.id);
            }
            return Ok(());
        }
        if let Some(turn) = render(m, &resolver, settings.tool_result_bytes)? {
            self.span.push((index, turn));
        }
        Ok(())
    }

    /// After the first full read: adopt a stored record of this same window whose anchors are
    /// still the window's own (a restarted follower picks up where the last one stopped).
    fn adopt_stored(&mut self, root: &Path) {
        let Ok(Some(record)) = store::load_record(root, self.r.harness.as_str(), &self.r.id) else {
            return;
        };
        let same_window = record.window.boundaries == self.window.boundaries.len()
            && record.cut.index < self.window.messages
            && record.anchors.len() <= self.window.anchors.len()
            && record
                .anchors
                .iter()
                .zip(&self.window.anchors)
                .all(|(a, p)| *a == p.anchor);
        if same_window {
            let cut = record.cut.index;
            self.span.retain(|(i, _)| *i > cut);
            self.record = Some(record);
        }
    }

    fn compact_at(&self, settings: &Settings, learned: &HashMap<(Harness, Option<String>), u64>) -> Option<u64> {
        settings
            .compact_at
            .or_else(|| self.window.compact_point())
            .or_else(|| learned.get(&(self.r.harness, self.window.model.clone())).copied())
    }

    /// The harness precomputes this session's compactions itself: its newest compaction says
    /// so, or its own configuration does. cv then stays idle for it (one method per session).
    fn native(&self) -> bool {
        self.window.native_precompute()
            || cv_core::harness::rootless(self.r.harness).and_then(|a| a.native_precompaction(&self.r)) == Some(true)
    }

    fn idle_secs(&self) -> u64 {
        self.window
            .newest_at
            .map_or(0, |t| (chrono::Utc::now() - t).num_seconds().max(0) as u64)
    }
}

/// What a summary job needs, taken from a [`Live`] when it is due.
struct Job {
    key: String,
    harness: String,
    session: String,
    prior: Option<String>,
    base: Option<pc::Cut>,
    turns: Vec<Turn>,
    cut: pc::Cut,
    anchors: Vec<pc::Anchor>,
    window: pc::WindowRef,
    profile: pc::Profile,
}

impl Job {
    fn take(live: &Live, profile: pc::Profile, extend: bool) -> Option<Job> {
        let (cut, anchors) = live.window.cut_now()?;
        let record = live.record.as_ref().filter(|_| extend);
        Some(Job {
            key: live.key(),
            harness: live.r.harness.as_str().to_owned(),
            session: live.r.id.clone(),
            prior: record.map(|r| r.text.clone()),
            base: record.map(|r| r.cut.clone()),
            turns: live
                .span
                .iter()
                .filter(|(i, _)| *i <= cut.index)
                .map(|(_, t)| t.clone())
                .collect(),
            cut,
            anchors,
            window: pc::WindowRef {
                boundaries: live.window.boundaries.len(),
                start: live.window.start,
            },
            profile,
        })
    }

    fn prompt_bytes(&self) -> usize {
        self.turns.iter().map(|t| t.content.len()).sum::<usize>() + self.prior.as_ref().map_or(0, String::len)
    }

    /// Call the summary model and build the record.
    fn run(self, settings: &Settings) -> Result<(Record, f64)> {
        let started = Instant::now();
        let turns = prompt::write(self.prior.as_deref(), self.turns, &self.profile.text);
        let (text, model) =
            crate::llm::complete(settings.model.as_deref(), &turns, SUMMARY_MAX_TOKENS, SUMMARY_TIMEOUT)?;
        anyhow::ensure!(!text.trim().is_empty(), "the summary model returned no text");
        let record = Record {
            v: pc::RECORD_VERSION,
            harness: self.harness,
            session: self.session,
            window: self.window,
            cut: self.cut,
            anchors: self.anchors,
            profile: self.profile.id,
            model,
            created: chrono::Utc::now(),
            base: self.base,
            bounds: vec![format!(
                "tool calls and results cut to {} bytes each",
                settings.tool_result_bytes
            )],
            text,
        };
        Ok((record, started.elapsed().as_secs_f64()))
    }
}

fn written_event(record: &Record, secs: f64, prompt_bytes: usize, span_turns: usize) -> Value {
    json!({
        "event": "written",
        "harness": record.harness,
        "session": record.session,
        "cut_index": record.cut.index,
        "cut_tokens": record.cut.tokens,
        "base_tokens": record.base.as_ref().and_then(|b| b.tokens),
        "anchors": record.anchors.len(),
        "span_turns": span_turns,
        "prompt_bytes": prompt_bytes,
        "text_bytes": record.text.len(),
        "model": record.model,
        "secs": (secs * 10.0).round() / 10.0,
    })
}

// ───────────────────────────── watch ─────────────────────────────

/// The directories to watch so a change to any followed session, or a new session beside them,
/// wakes the loop: every directory from a store root down to each session's parent. A new
/// directory at a depth where sessions live (or above it) is watched when it appears.
struct Dirs {
    /// Store root → the depth (in path components below it) of a session's parent directory.
    roots: Vec<(PathBuf, usize)>,
}

impl Dirs {
    fn new(harnesses: &[Harness], refs: &[SessionRef]) -> Dirs {
        let mut roots = Vec::new();
        for h in harnesses {
            // The adapter as discovery configures it: its roots are where sessions are found.
            let Some(adapter) = cv_core::harness::for_harness(*h) else {
                continue;
            };
            for root in adapter.roots() {
                let depth = refs
                    .iter()
                    .filter_map(|r| r.path.parent()?.strip_prefix(&root).ok())
                    .map(|rel| rel.components().count())
                    .max();
                if let Some(depth) = depth {
                    roots.push((root, depth));
                }
            }
        }
        Dirs { roots }
    }

    /// The root `dir` lies under and its depth below it.
    fn place(&self, dir: &Path) -> Option<(&Path, usize, usize)> {
        self.roots.iter().find_map(|(root, depth)| {
            let rel = dir.strip_prefix(root).ok()?;
            Some((root.as_path(), rel.components().count(), *depth))
        })
    }

    /// Watch every directory from each root down to where sessions live, so a new session in a
    /// project with none followed yet wakes the loop. Bounded by the depth sessions were found at.
    fn watch_all(&self, wake: &mut cv_core::watch::wake::Wake) {
        for (root, depth) in &self.roots {
            let mut level = vec![root.clone()];
            for at in 0..=*depth {
                let mut next = Vec::new();
                for dir in &level {
                    let _ = wake.watch(dir);
                    if at < *depth {
                        let Ok(rd) = std::fs::read_dir(dir) else { continue };
                        next.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
                    }
                }
                level = next;
            }
        }
    }

    /// Watch the chain from `r`'s root to its parent.
    fn watch_session(&self, wake: &mut cv_core::watch::wake::Wake, r: &SessionRef) {
        let Some(parent) = r.path.parent() else { return };
        let Some((root, _, _)) = self.place(parent) else {
            let _ = wake.watch(parent);
            return;
        };
        let mut dir = parent.to_path_buf();
        loop {
            let _ = wake.watch(&dir);
            if dir == root {
                break;
            }
            if !dir.pop() {
                break;
            }
        }
    }
}

fn watch(
    harness: Option<Harness>,
    cwd: Option<String>,
    dry_run: bool,
    for_: Option<&str>,
    settings: Settings,
) -> Result<()> {
    use cv_core::watch::{wake::Wake, Filter, Watcher};
    use std::cell::RefCell;
    use std::rc::Rc;

    let root = store::root()?;
    let wait = Wait::resolve(true, for_)?;
    let filter = Filter {
        harness,
        cwd_contains: cwd,
    };
    let started = Instant::now();
    // Every session of the harnesses in scope places the store roots, so a filter that matches
    // nothing yet (a new project's `--cwd`) still watches for the session that will.
    let all: Vec<SessionRef> = cv_core::sessions()
        .into_iter()
        .filter(|r| filter.harness.is_none_or(|h| h == r.harness))
        .collect();
    let live_within = chrono::Duration::seconds(i64::try_from(settings.live_within_secs).unwrap_or(i64::MAX));
    let recent = |r: &SessionRef| {
        r.updated_at
            .or(r.created_at)
            .is_some_and(|t| chrono::Utc::now() - t <= live_within)
    };
    let mut harnesses: Vec<Harness> = all.iter().map(|r| r.harness).collect();
    harnesses.sort_by_key(|h| h.as_str());
    harnesses.dedup();
    let dirs = Dirs::new(&harnesses, &all);
    let mut followed: Vec<SessionRef> = Vec::new();
    for r in all.iter().filter(|r| filter.matches(r) && recent(r)) {
        // One session reached through linked project directories is followed once.
        if !followed.iter().any(|f| f.harness == r.harness && f.id == r.id) {
            followed.push(r.clone());
        }
    }
    drop(all);
    let follow = Rc::new(RefCell::new(followed));
    let mut wake = Wake::new(Duration::from_secs(3))?;
    for r in follow.borrow().iter() {
        dirs.watch_session(&mut wake, r);
    }
    dirs.watch_all(&mut wake);
    let mut watcher = Watcher::with_discovery(filter.clone(), true, {
        let follow = follow.clone();
        move || follow.borrow().clone()
    });
    eprintln!(
        "cv precompact watch: following {} live session(s) (touched within {}s), watching {} director{}{}",
        follow.borrow().len(),
        settings.live_within_secs,
        wake.watched(),
        if wake.watched() == 1 { "y" } else { "ies" },
        if dry_run {
            "; dry run: no model calls, no records"
        } else {
            ""
        },
    );

    let mut lives: HashMap<String, Live> = HashMap::new();
    let mut learned: HashMap<(Harness, Option<String>), u64> = HashMap::new();
    let (tx, rx) = std::sync::mpsc::channel::<(String, Result<(Record, f64)>, usize, usize)>();
    let waker = wake.waker();
    let mut running = 0usize;
    let mut first = true;
    // Sessions waiting out a retry after a failed read: when they are followed again.
    let mut held: HashMap<String, (SessionRef, Instant, u32)> = HashMap::new();
    let mut retries: HashMap<String, u32> = HashMap::new();
    let mut looked_up: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Whether the last wake can have changed a followed session: a write elsewhere in a watched
    // directory (another project's session) reads nothing.
    let mut poll_now = true;
    loop {
        // Sessions whose retry is due are followed again.
        let now = Instant::now();
        let due: Vec<String> = held
            .iter()
            .filter(|(_, h)| h.1 <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            if let Some((r, _, _)) = held.remove(&k) {
                looked_up.remove(&r.path);
                follow.borrow_mut().push(r);
                poll_now = true;
            }
        }
        // Read what changed, one message at a time. A session first seen is read whole in one
        // pass (its window and the compactions that came before); after that only what was
        // appended. A batched poll re-read a session from its last checkpoint to EOF once per
        // batch: quadratic on a long session, and a live fleet never finished priming.
        if poll_now {
            loop {
                let mut errors = Vec::new();
                let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut touched: Vec<String> = Vec::new();
                let polled = watcher.try_poll_messages(
                    || Ok(()),
                    |kind, r, m| {
                        let key = format!("{}:{}", r.harness.as_str(), r.id);
                        let live = lives.entry(key.clone()).or_insert_with(|| Live::new(r.clone()));
                        live.r = r.clone();
                        if touched.last() != Some(&key) {
                            touched.push(key.clone());
                            // The watcher forgets a session that left its discovery (a retry wait, or
                            // idle past the live window) and reads it from the start when it is back.
                            if kind == cv_core::watch::EventKind::New {
                                live.restart();
                            }
                        }
                        if !live.take() || failed.contains(&key) {
                            return Ok(cv_core::Flow::Continue);
                        }
                        if let Err(e) = live.observe(&m, &settings, &root, dry_run) {
                            errors.push(format!("{}: {e:#}", short_id(&r.id)));
                            failed.insert(key);
                        }
                        Ok(cv_core::Flow::Continue)
                    },
                );
                for e in errors {
                    eprintln!("cv precompact: {e}");
                }
                // A read stops at the first session that fails; every session read before it was
                // accepted. The failed one is re-delivered from its accepted position, so what was
                // observed of it is skipped then, and it waits out a doubling retry.
                let why = polled.as_ref().err().map(|e| format!("{e:#}"));
                for key in &touched {
                    let Some(live) = lives.get_mut(key) else { continue };
                    let path = live.r.path.display().to_string();
                    let read = !why.as_deref().is_some_and(|w| w.contains(&path));
                    live.settle(read);
                    if read {
                        retries.remove(key);
                    }
                }
                match (polled, why) {
                    (Ok(true), _) => continue,
                    (Ok(false), _) => break,
                    (Err(_), Some(why)) => {
                        let mut follow = follow.borrow_mut();
                        let mut gone: Vec<SessionRef> = follow
                            .iter()
                            .filter(|r| why.contains(&r.path.display().to_string()))
                            .cloned()
                            .collect();
                        gone.dedup_by(|a, b| a.harness == b.harness && a.id == b.id);
                        if gone.is_empty() {
                            eprintln!("cv precompact: {why}");
                            break;
                        }
                        // Every path to the session waits, not only the one that failed: the same
                        // transcript under a linked project directory is the same read.
                        follow.retain(|r| !gone.iter().any(|g| g.harness == r.harness && g.id == r.id));
                        for r in gone {
                            let key = format!("{}:{}", r.harness.as_str(), r.id);
                            let n = retries.entry(key.clone()).or_insert(0);
                            *n += 1;
                            let wait = RETRY_FIRST.saturating_mul(1 << (*n - 1).min(16)).min(RETRY_MAX);
                            eprintln!(
                                "cv precompact: {}: {why}; retried in {}s",
                                short_id(&r.id),
                                wait.as_secs()
                            );
                            held.insert(key, (r, Instant::now() + wait, *n));
                        }
                    }
                    (Err(_), None) => unreachable!("an error has its text"),
                }
            }
        }
        for live in lives.values_mut() {
            if !live.primed {
                live.primed = true;
                if !dry_run {
                    live.adopt_stored(&root);
                }
            }
            if let Some(point) = live.window.compact_point() {
                let slot = learned
                    .entry((live.r.harness, live.window.model.clone()))
                    .or_insert(point);
                *slot = (*slot).min(point);
            }
        }
        if first {
            first = false;
            eprintln!(
                "cv precompact watch: primed {} session(s) in {:.1}s",
                lives.len(),
                started.elapsed().as_secs_f64()
            );
        }

        // Finished jobs.
        while let Ok((key, outcome, prompt_bytes, span_turns)) = rx.try_recv() {
            running -= 1;
            let Some(live) = lives.get_mut(&key) else { continue };
            live.job = false;
            match outcome {
                Ok((record, secs)) => {
                    let same_window = record.window.boundaries == live.window.boundaries.len();
                    let _ = store::event(&root, written_event(&record, secs, prompt_bytes, span_turns));
                    if same_window {
                        if let Err(e) = store::save_record(&root, &record) {
                            eprintln!("cv precompact: saving {}: {e:#}", short_id(&record.session));
                            continue;
                        }
                        eprintln!(
                            "cv precompact: {} summary to msg {} ({} tokens) in {:.1}s",
                            short_id(&record.session),
                            record.cut.index,
                            record.cut.tokens.map_or("?".into(), fmt_int),
                            secs
                        );
                        let cut = record.cut.index;
                        live.span.retain(|(i, _)| *i > cut);
                        live.record = Some(record);
                    }
                }
                Err(e) => {
                    eprintln!("cv precompact: {} summary failed: {e:#}", short_id(&live.r.id));
                    let _ = store::event(
                        &root,
                        json!({"event": "failed", "harness": live.r.harness.as_str(), "session": live.r.id, "error": format!("{e:#}")}),
                    );
                }
            }
        }

        // Decide, and start what is due.
        let mut next_idle: Option<u64> = None;
        let mut profiles: HashMap<Harness, Option<pc::Profile>> = HashMap::new();
        for live in lives.values_mut() {
            if live.job {
                continue;
            }
            if live.native() {
                if live.said != "native" {
                    live.said = "native";
                    eprintln!(
                        "cv precompact: {}'s harness precomputes its own compactions; leaving it alone",
                        short_id(&live.r.id)
                    );
                }
                continue;
            }
            let idle = live.idle_secs();
            let compact_at = live.compact_at(&settings, &learned);
            let due = pc::due(
                &settings,
                &live.window,
                compact_at,
                live.record.as_ref().map(|r| &r.cut),
                idle,
            );
            let extend = match due {
                pc::Due::Wait(why) => {
                    if why == "waiting for more tokens or for the session to go idle" {
                        let left = settings.idle_secs.saturating_sub(idle);
                        next_idle = Some(next_idle.map_or(left, |n| n.min(left)));
                    }
                    if live.said != why
                        && live.primed
                        && why != "below the start threshold"
                        && why != "no usage record yet"
                    {
                        live.said = why;
                    }
                    continue;
                }
                pc::Due::Start => false,
                pc::Due::Extend => true,
            };
            if running >= settings.jobs {
                next_idle = Some(1);
                continue;
            }
            let profile = profiles
                .entry(live.r.harness)
                .or_insert_with(|| store::latest_profile(&root, live.r.harness.as_str()).ok().flatten())
                .clone();
            let Some(profile) = profile else {
                if live.said != "no profile" {
                    live.said = "no profile";
                    eprintln!(
                        "cv precompact: {} is due but no {} compaction instruction is captured yet: the first compaction \
                         answered through `cv precompact answer` captures it, or `cv precompact profile import`",
                        short_id(&live.r.id),
                        live.r.harness
                    );
                }
                continue;
            };
            let Some(job) = Job::take(live, profile, extend) else {
                continue;
            };
            let (prompt_bytes, span_turns) = (job.prompt_bytes(), job.turns.len());
            if dry_run {
                let _ = store::event(
                    &root,
                    json!({"event": "due", "dry_run": true, "harness": job.harness, "session": job.session,
                           "extend": extend, "cut_index": job.cut.index, "cut_tokens": job.cut.tokens,
                           "compact_at": compact_at, "span_turns": span_turns, "prompt_bytes": prompt_bytes}),
                );
                eprintln!(
                    "cv precompact: would {} {} to msg {} ({} tokens of {} compact point; {} turns, {} bytes)",
                    if extend { "extend" } else { "start" },
                    short_id(&job.session),
                    job.cut.index,
                    job.cut.tokens.map_or("?".into(), fmt_int),
                    compact_at.map_or("?".into(), fmt_int),
                    span_turns,
                    fmt_int(prompt_bytes as u64)
                );
                // Stand in for the record so the dry run's cadence matches a real one.
                live.record = Some(Record {
                    v: pc::RECORD_VERSION,
                    harness: job.harness,
                    session: job.session,
                    window: job.window,
                    cut: job.cut.clone(),
                    anchors: Vec::new(),
                    profile: job.profile.id,
                    model: String::new(),
                    created: chrono::Utc::now(),
                    base: None,
                    bounds: Vec::new(),
                    text: String::new(),
                });
                let cut = job.cut.index;
                live.span.retain(|(i, _)| *i > cut);
                continue;
            }
            live.job = true;
            running += 1;
            let (tx, waker, settings) = (tx.clone(), waker.clone(), settings.clone());
            let env = cv_core::env::Env::current();
            std::thread::spawn(move || {
                let _scope = cv_core::env::Env::reenter(env);
                let key = job.key.clone();
                let outcome = job.run(&settings);
                let _ = tx.send((key, outcome, prompt_bytes, span_turns));
                waker.wake();
            });
        }

        if wait.expired() {
            return Ok(());
        }
        let timeout = match (&wait, next_idle) {
            (Wait::Until(Some((deadline, _))), idle) => {
                let left = deadline.saturating_duration_since(Instant::now());
                Some(idle.map_or(left, |s| left.min(Duration::from_secs(s.max(1)))))
            }
            (_, idle) => idle.map(|s| Duration::from_secs(s.max(1))),
        };
        let retry = held
            .values()
            .map(|h| {
                h.1.saturating_duration_since(Instant::now())
                    .max(Duration::from_secs(1))
            })
            .min();
        let timeout = match (timeout, retry) {
            (Some(t), Some(r)) => Some(t.min(r)),
            (t, r) => t.or(r),
        };
        let woke = wake.wait(timeout)?;
        // A new directory where sessions live is watched; a new file in one may be a new session.
        // A file is looked up once: a session cv does not follow (filtered out, waiting a retry,
        // or the same transcript under a linked directory) is a write in a watched directory on
        // every append, and must not cost a discovery each time.
        let mut rediscover = woke.overflow;
        let known: std::collections::HashSet<PathBuf> = follow.borrow().iter().map(|r| r.path.clone()).collect();
        let mut new_files = Vec::new();
        for path in &woke.paths {
            if known.contains(path) || looked_up.contains(path) {
                continue;
            }
            let Some(parent) = path.parent() else { continue };
            match dirs.place(parent) {
                Some((_, at, depth)) if path.is_dir() && at < depth => {
                    let _ = wake.watch(path);
                    rediscover = true;
                }
                Some((_, at, depth)) if at == depth && path.is_file() => {
                    new_files.push(path.clone());
                    rediscover = true;
                }
                _ => {}
            }
        }
        looked_up.extend(new_files);
        poll_now = woke.paths.is_empty() || rediscover || woke.paths.iter().any(|p| known.contains(p));
        if rediscover {
            let fresh: Vec<SessionRef> = cv_core::sessions()
                .into_iter()
                .filter(|r| filter.matches(r) && recent(r))
                .collect();
            let mut follow = follow.borrow_mut();
            let added: Vec<SessionRef> = fresh
                .into_iter()
                .filter(|r| !follow.iter().any(|f| f.harness == r.harness && f.id == r.id))
                .filter(|r| !held.contains_key(&format!("{}:{}", r.harness.as_str(), r.id)))
                .collect();
            for r in &added {
                dirs.watch_session(&mut wake, r);
                eprintln!("cv precompact: following new session {}", short_id(&r.id));
            }
            follow.extend(added);
            // A followed file is known by the follow list; one let go later is looked up again.
            looked_up.retain(|p| !follow.iter().any(|r| &r.path == p));
        }
        // Sessions idle past the live window are no longer read. What cv holds for one stays, so
        // a session that comes back is read on from where it was: the watcher keeps its position.
        let stale: Vec<String> = lives
            .iter()
            .filter(|(_, l)| !l.job && l.primed && l.idle_secs() > settings.live_within_secs)
            .map(|(k, _)| k.clone())
            .collect();
        if !stale.is_empty() {
            follow
                .borrow_mut()
                .retain(|r| !stale.contains(&format!("{}:{}", r.harness.as_str(), r.id)));
        }
    }
}

// ───────────────────────────── now / check ─────────────────────────────

/// Read `r` (its first `upto_bytes` bytes when given) into a [`Live`].
fn read_live(
    r: &SessionRef,
    adapter: &dyn cv_core::Adapter,
    upto_bytes: Option<u64>,
    settings: &Settings,
    root: &Path,
    mut each: impl FnMut(&Live, &Message),
) -> Result<Live> {
    let mut live = Live::new(r.clone());
    let mut error = None;
    let mut sink = |m: Message| {
        if let Err(e) = live.observe(&m, settings, root, true) {
            error = Some(e);
            return cv_core::Flow::Stop;
        }
        each(&live, &m);
        cv_core::Flow::Continue
    };
    match upto_bytes {
        Some(len) => {
            adapter
                .transcript(
                    r,
                    &cv_core::Demand::RICH,
                    cv_core::stream::Start::Prefix(len),
                    &mut sink,
                )?
                .with_context(|| format!("{} cannot read a byte prefix of {}", r.harness, r.path.display()))?;
        }
        None => {
            adapter.try_stream(r, &cv_core::Demand::RICH, &mut sink)?;
        }
    }
    if let Some(e) = error {
        return Err(e);
    }
    live.primed = true;
    Ok(live)
}

fn now(id: &str, harness: Option<Harness>, dry_run: bool, settings: &Settings) -> Result<()> {
    let (r, adapter) = crate::cmd::distill::resolve_source(id, harness)?;
    let root = store::root()?;
    let mut live = read_live(&r, adapter.as_ref(), None, settings, &root, |_, _| {})?;
    if live.native() {
        eprintln!(
            "cv precompact: note: {}'s harness precomputes its own compactions",
            short_id(&r.id)
        );
    }
    live.adopt_stored(&root);
    let Some(profile) = store::latest_profile(&root, r.harness.as_str())? else {
        return usage(format!(
            "no {} compaction instruction is captured yet: `cv precompact profile import <request.json> --harness {}`",
            r.harness, r.harness
        ));
    };
    let extend = live.record.is_some();
    let job = Job::take(&live, profile, extend).context("the session's window is empty")?;
    eprintln!(
        "cv precompact: {} {} to msg {} ({} tokens): {} turns, {} bytes",
        if extend { "extending" } else { "writing" },
        short_id(&r.id),
        job.cut.index,
        job.cut.tokens.map_or("?".into(), fmt_int),
        job.turns.len(),
        fmt_int(job.prompt_bytes() as u64)
    );
    if dry_run {
        return Ok(());
    }
    let (prompt_bytes, span_turns) = (job.prompt_bytes(), job.turns.len());
    let (record, secs) = job.run(settings)?;
    store::event(&root, written_event(&record, secs, prompt_bytes, span_turns))?;
    let path = store::save_record(&root, &record)?;
    eprintln!("cv precompact: wrote {} in {secs:.1}s", path.display());
    Ok(())
}

fn read_request(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

fn check(
    request: &Path,
    id: &str,
    harness: Option<Harness>,
    upto_bytes: Option<u64>,
    cut_tokens: Option<u64>,
) -> Result<()> {
    let input = read_request(request)?;
    let view = pc::RequestView::parse(&input)?;
    let (r, adapter) = crate::cmd::distill::resolve_source(id, harness)?;
    let settings = Settings::default();
    let root = std::env::temp_dir();
    // With --cut-tokens, the cut is the newest message at which the fill stood at or under it.
    let mut snapshot: Option<(pc::Cut, usize, usize)> = None;
    // Each message's id, kind and opening text, so a miss can name the message it stopped at.
    let mut seen: Vec<(Option<String>, MessageKind, String)> = Vec::new();
    let live = read_live(&r, adapter.as_ref(), upto_bytes, &settings, &root, |live, m| {
        let mut head = m.text().unwrap_or_default();
        head.truncate(head.floor_char_boundary(160));
        seen.push((m.id.clone(), m.kind, head));
        if let (Some(limit), Some(fill), Some(cut)) = (cut_tokens, live.window.fill(), live.window.cut()) {
            if fill <= limit {
                snapshot = Some((cut, live.window.anchors.len(), live.window.boundaries.len()));
            }
        }
    })?;
    let (cut, anchors) = match (cut_tokens, snapshot) {
        (None, _) => live.window.cut_now().context("the session's window is empty")?,
        (Some(_), Some((cut, n, boundaries))) if boundaries == live.window.boundaries.len() => {
            (cut, live.window.anchors[..n].iter().map(|p| p.anchor.clone()).collect())
        }
        (Some(t), _) => anyhow::bail!("the window never stood at or under {t} tokens"),
    };
    let record = Record {
        v: pc::RECORD_VERSION,
        harness: r.harness.as_str().into(),
        session: r.id.clone(),
        window: pc::WindowRef {
            boundaries: live.window.boundaries.len(),
            start: live.window.start,
        },
        cut: cut.clone(),
        anchors,
        profile: view
            .instruction
            .as_ref()
            .map_or_else(String::new, |i| pc::profile_id(&i.text)),
        model: String::new(),
        created: chrono::Utc::now(),
        base: None,
        bounds: Vec::new(),
        text: String::new(),
    };
    let verdict = pc::validate(&view, &record);
    let mut out = json!({
        "session": r.id,
        "request_session": view.session.as_ref().map(|s| &s.id),
        "request_session_source": view.session.as_ref().map(|s| &s.source),
        "session_matches": view.session.as_ref().is_some_and(|s| s.id == r.id),
        "request_messages": view.messages.len(),
        "instruction_profile": record.profile,
        "window_messages": live.window.messages - live.window.start,
        "anchors": record.anchors.len(),
        "cut_index": cut.index,
        "cut_tokens": cut.tokens,
        "cut_clean": cut.clean,
        "fill": live.window.fill(),
    });
    match verdict {
        Ok(m) => {
            let tail = view.tail(m.tail_from);
            out["verdict"] = json!("match");
            out["tail_from"] = json!(m.tail_from);
            out["tail_messages"] = json!(tail.len());
            out["tail_text_bytes"] =
                json!(tail
                    .iter()
                    .map(|t| t.texts.iter().map(String::len).sum::<usize>()
                        + t.notes.iter().map(String::len).sum::<usize>())
                    .sum::<usize>());
        }
        Err(miss) => {
            out["verdict"] = json!("miss");
            out["reason"] = json!(miss.to_string());
            if let pc::Miss::Anchor { index, .. } = miss {
                if let Some((id, kind, head)) = live.window.anchors.get(index).and_then(|p| seen.get(p.index)) {
                    out["miss_message"] = json!({"id": id, "kind": kind, "text": head});
                }
            }
        }
    }
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

// ───────────────────────────── answer ─────────────────────────────

fn miss(root: &Path, harness: Option<&str>, session: Option<&str>, reason: &str, started: Instant) -> Result<()> {
    let _ = store::event(
        root,
        json!({"event": "answer", "outcome": "miss", "harness": harness, "session": session,
               "reason": reason, "secs": started.elapsed().as_secs_f64()}),
    );
    eprintln!("cv precompact answer: miss: {reason}");
    std::process::exit(MISS_EXIT);
}

fn answer(harness: Option<String>, no_tail_pass: bool, model: Option<String>) -> Result<()> {
    let started = Instant::now();
    let root = store::root()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading the request on stdin")?;
    let input: Value = serde_json::from_str(&text).context("the request on stdin is not JSON")?;
    drop(text);
    let view = pc::RequestView::parse(&input)?;
    drop(input);
    let harness = harness.or_else(|| view.harness.clone());
    let Some(session) = view.session.clone() else {
        return miss(&root, harness.as_deref(), None, "the request names no session", started);
    };
    // Every compaction request through here teaches cv its harness's instruction.
    if let (Some(ins), Some(h)) = (&view.instruction, &harness) {
        let profile = pc::Profile {
            id: pc::profile_id(&ins.text),
            harness: h.clone(),
            client: view.client.clone(),
            captured: chrono::Utc::now(),
            text: ins.text.clone(),
        };
        if let Ok(true) = store::save_profile(&root, &profile) {
            eprintln!("cv precompact answer: captured {h} profile {}", profile.id);
        }
    }
    let record = match &harness {
        Some(h) => store::load_record(&root, h, &session.id)?,
        None => store::records(&root, None)?
            .into_iter()
            .map(|(_, r)| r)
            .find(|r| r.session == session.id),
    };
    let Some(record) = record else {
        return miss(
            &root,
            harness.as_deref(),
            Some(&session.id),
            "no precomputed summary for this session",
            started,
        );
    };
    let matched = match pc::validate(&view, &record) {
        Ok(m) => m,
        Err(why) => {
            return miss(
                &root,
                Some(&record.harness),
                Some(&session.id),
                &why.to_string(),
                started,
            )
        }
    };
    let tail = view.tail(matched.tail_from);
    let mut tail_secs = None;
    let text = if tail.is_empty() {
        record.text.clone()
    } else if no_tail_pass {
        return miss(
            &root,
            Some(&record.harness),
            Some(&session.id),
            &format!("{} message(s) follow the cut and --no-tail-pass is set", tail.len()),
            started,
        );
    } else {
        let settings = cv_core::config::load().precompact.unwrap_or_default();
        let model = model.or(settings.model.clone());
        let pass = Instant::now();
        let turns = prompt::tail(&record.text, &tail, settings.tool_result_bytes);
        match crate::llm::complete(model.as_deref(), &turns, TAIL_MAX_TOKENS, TAIL_TIMEOUT) {
            Ok((update, _)) => {
                tail_secs = Some(pass.elapsed().as_secs_f64());
                prompt::splice(&record.text, &update)
            }
            Err(e) => {
                return miss(
                    &root,
                    Some(&record.harness),
                    Some(&session.id),
                    &format!("the tail pass failed: {e:#}"),
                    started,
                )
            }
        }
    };
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    let _ = store::event(
        &root,
        json!({"event": "answer", "outcome": "served", "harness": record.harness, "session": session.id,
               "session_source": session.source, "anchors": matched.anchors, "tail_messages": tail.len(),
               "tail_pass_secs": tail_secs, "record_cut_tokens": record.cut.tokens,
               "record_age_secs": (chrono::Utc::now() - record.created).num_seconds(),
               "text_bytes": text.len(), "secs": started.elapsed().as_secs_f64()}),
    );
    eprintln!(
        "cv precompact answer: served {} ({} anchors, {} tail message(s){}) in {:.2}s",
        short_id(&session.id),
        matched.anchors,
        tail.len(),
        tail_secs.map_or(String::new(), |s| format!(", tail pass {s:.1}s")),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

// ───────────────────────────── profiles, ls, report ─────────────────────────────

fn profile(action: ProfileCmd) -> Result<()> {
    let root = store::root()?;
    match action {
        ProfileCmd::Import { request, harness } => {
            let view = pc::RequestView::parse(&read_request(&request)?)?;
            let ins = view
                .instruction
                .context("the request has no user text to take as the compaction instruction")?;
            let profile = pc::Profile {
                id: pc::profile_id(&ins.text),
                harness,
                client: view.client,
                captured: chrono::Utc::now(),
                text: ins.text,
            };
            let new = store::save_profile(&root, &profile)?;
            println!(
                "{} {} profile {} ({} bytes)",
                if new { "captured" } else { "already had" },
                profile.harness,
                profile.id,
                profile.text.len()
            );
        }
        ProfileCmd::Ls => {
            for p in store::profiles(&root)? {
                let head: String = p.text.chars().take(72).collect::<String>().replace('\n', " ");
                println!(
                    "{}  {:<8} {}  {}  {head}…",
                    p.id,
                    p.harness,
                    p.captured.format("%Y-%m-%d %H:%M"),
                    p.client.as_deref().unwrap_or("-")
                );
            }
        }
    }
    Ok(())
}

fn ls(json: bool) -> Result<()> {
    let root = store::root()?;
    let records = store::records(&root, None)?;
    if json {
        let rows: Vec<Value> = records
            .iter()
            .map(|(path, r)| {
                json!({"harness": r.harness, "session": r.session, "cut": r.cut, "window": r.window,
                       "anchors": r.anchors.len(), "profile": r.profile, "model": r.model,
                       "created": r.created, "text_bytes": r.text.len(), "path": path})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no precomputed summaries in {}", root.display());
    }
    for (_, r) in records {
        println!(
            "{:<8} {}  msg {} · {} tokens · {} anchors · {} bytes · {} · {}",
            r.harness,
            short_id(&r.session),
            r.cut.index,
            r.cut.tokens.map_or("?".into(), fmt_int),
            r.anchors.len(),
            fmt_int(r.text.len() as u64),
            r.model,
            r.created.format("%Y-%m-%d %H:%M"),
        );
    }
    Ok(())
}

fn median(v: &mut [u64]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

fn report(json_out: bool) -> Result<()> {
    let root = store::root()?;
    let text = match std::fs::read_to_string(root.join("events.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    // Per session, the newest answer outcome since its last compaction decides how the next
    // compaction was served.
    let mut last_answer: HashMap<String, String> = HashMap::new();
    let mut stalls: HashMap<&'static str, Vec<u64>> = HashMap::new();
    let (mut served, mut missed, mut written) = (0u64, 0u64, 0u64);
    let mut reasons: HashMap<String, u64> = HashMap::new();
    let mut tail_secs = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let session = v.get("session").and_then(Value::as_str).unwrap_or("").to_owned();
        match v.get("event").and_then(Value::as_str) {
            Some("answer") => {
                let outcome = v.get("outcome").and_then(Value::as_str).unwrap_or("").to_owned();
                if outcome == "served" {
                    served += 1;
                    if let Some(s) = v.get("tail_pass_secs").and_then(Value::as_f64) {
                        tail_secs.push((s * 1000.0) as u64);
                    }
                } else {
                    missed += 1;
                    let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
                    let class = reason
                        .split(':')
                        .next()
                        .unwrap_or(reason)
                        .chars()
                        .take(60)
                        .collect::<String>();
                    *reasons.entry(class).or_default() += 1;
                }
                last_answer.insert(session, outcome);
            }
            Some("written") => written += 1,
            Some("compaction") => {
                let method = if v.get("native").and_then(Value::as_bool) == Some(true) {
                    "native precompute"
                } else if last_answer.get(&session).map(String::as_str) == Some("served") {
                    "cv precompact"
                } else {
                    "harness's own"
                };
                last_answer.remove(&session);
                if let Some(ms) = v.get("duration_ms").and_then(Value::as_u64) {
                    stalls.entry(method).or_default().push(ms);
                }
            }
            _ => {}
        }
    }
    let rows: Vec<Value> = ["cv precompact", "native precompute", "harness's own"]
        .iter()
        .map(|m| {
            let mut v = stalls.get(m).cloned().unwrap_or_default();
            json!({"method": m, "compactions": v.len(), "median_ms": median(&mut v), "max_ms": v.iter().max()})
        })
        .collect();
    let out = json!({"summaries_written": written, "answers_served": served, "answers_missed": missed,
                     "miss_reasons": reasons, "tail_pass_median_ms": median(&mut tail_secs), "stalls": rows});
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("summaries written {written} · answers served {served} · missed {missed}");
    for (reason, n) in &reasons {
        println!("  miss {n}× {reason}");
    }
    for row in rows {
        println!(
            "{:<18} {:>4} compaction(s) · median {} · max {}",
            row["method"].as_str().unwrap_or(""),
            row["compactions"],
            row["median_ms"].as_u64().map_or("-".into(), crate::util::fmt_duration),
            row["max_ms"].as_u64().map_or("-".into(), crate::util::fmt_duration),
        );
    }
    Ok(())
}

// ───────────────────────────── swap (no proxy) ─────────────────────────────

fn swap(id: &str, harness: Option<Harness>, dry_run: bool) -> Result<()> {
    let (r, adapter) = crate::cmd::distill::resolve_source(id, harness)?;
    let root = store::root()?;
    let Some(record) = store::load_record(&root, r.harness.as_str(), &r.id)? else {
        return usage(format!("no precomputed summary for {}", short_id(&r.id)));
    };
    // The transcript's own check: the record's anchors are still this window's.
    let started = Instant::now();
    let live = read_live(&r, adapter.as_ref(), None, &Settings::default(), &root, |_, _| {})?;
    let holds = record.window.boundaries == live.window.boundaries.len()
        && record.anchors.len() <= live.window.anchors.len()
        && record
            .anchors
            .iter()
            .zip(&live.window.anchors)
            .all(|(a, p)| *a == p.anchor);
    if !holds {
        return usage(format!(
            "{}'s summary no longer matches its transcript (it compacted or was rewound since)",
            short_id(&r.id)
        ));
    }
    let cut_id = record
        .cut
        .id
        .as_deref()
        .context("the record's cut has no message id to splice after")?;
    let Some(copy) = adapter.compacted_copy(&r, cut_id, &record.text, record.cut.tokens) else {
        return usage(format!("{} has no compacted-copy writer yet", r.harness));
    };
    let copy = copy?;
    eprintln!(
        "cv precompact swap: {} -> {} (summary to msg {}, {} record(s) after the cut, {} bytes) in {:.2}s",
        short_id(&r.id),
        short_id(&copy.id),
        record.cut.index,
        copy.carried,
        fmt_int(copy.bytes.len() as u64),
        started.elapsed().as_secs_f64()
    );
    if dry_run {
        return Ok(());
    }
    anyhow::ensure!(!cv_core::env::frozen(), "--frozen forbids writing a session");
    cv_core::publish::publish_reader(
        &copy.path,
        "precompact-swap",
        copy.bytes.as_slice(),
        cv_core::publish::Claim::New,
    )?;
    println!("{}", copy.path.display());
    for line in crate::cmd::port::resume_lines(r.harness, &copy.id, r.cwd.as_deref()) {
        eprintln!("  {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read that fails after delivering messages is delivered again from the last accepted
    /// position; what was observed of it is skipped, and only the new messages are taken.
    #[test]
    fn a_failed_read_is_taken_once() {
        let r = SessionRef::new("s", Harness::Claude, Path::new("/s.jsonl"));
        let mut live = Live::new(r);
        let took = |live: &mut Live, n: usize| (0..n).filter(|_| live.take()).count();
        assert_eq!(took(&mut live, 5), 5);
        live.settle(true);
        assert_eq!(took(&mut live, 3), 3);
        live.settle(false);
        // Delivered again: the 3 already taken, then 2 new; then it fails once more.
        assert_eq!(took(&mut live, 4), 1);
        live.settle(false);
        assert_eq!(took(&mut live, 6), 2);
        live.settle(true);
        assert_eq!((live.delivered, live.accepted, live.redelivered), (11, 11, 0));
        // Let go and back: read from the start, so all 11 come again before 1 new one.
        live.restart();
        assert_eq!(took(&mut live, 12), 1);
        live.settle(true);
        assert_eq!((live.delivered, live.accepted, live.redelivered), (12, 12, 0));
        // A first read that fails is read from the start again, skipped as far as it got.
        let mut live = Live::new(SessionRef::new("t", Harness::Claude, Path::new("/t.jsonl")));
        assert_eq!(took(&mut live, 4), 4);
        live.settle(false);
        live.restart();
        assert_eq!(took(&mut live, 6), 2);
    }
}
