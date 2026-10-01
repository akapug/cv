//! `cv task` — the fleet task substrate: open/claim/propose/review/verify dispatch objects whose
//! landing state is *observed* from git, never taken on an agent's word.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{self, InboxReason, InboxRow, TaskEventKind, TaskProjection, TaskReadModel, TaskRow, TaskStore};

use crate::util::fmt_local;

/// A text argument that may come from a file: `--body-file`/`--file` (`-` reads stdin). Bodies
/// were 500-char shell strings before this existed; a brief is a document.
fn read_text_arg(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading the text from stdin")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// `a,b, c` → `["a", "b", "c"]`: trimmed, empties dropped, duplicates collapsed in order.
fn parse_tags(csv: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in csv.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !out.iter().any(|have| have == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// The id-prefix length that keeps every task in `model` distinguishable (never below 8). Task
/// ids are UUID v7: a batch opened in one second shares its first eight hex digits, so the usual
/// 8-char prefix was ambiguous for every task opened that way — `cv task show <prefix>` refused
/// what `cv task list` had just printed.
fn prefix_len(model: &TaskReadModel) -> usize {
    task::unique_prefix_len(model.tasks.keys().map(String::as_str), 8)
}

fn prefix(id: &str, len: usize) -> &str {
    id.get(..len).unwrap_or(id)
}

/// Identity resolution (G4) for chat-grade verbs: explicit `--from`, else `CV_ENDPOINT`, else
/// the literal `"cv"` (a human opening a task from a shell owes no ceremony). Shared logic lives
/// in [`task::actor`]; only the CLI's default sink is chosen here.
fn from_or_cv(explicit: Option<String>) -> String {
    task::actor(explicit, "cv")
}

/// Identity resolution for identity-BEARING verbs (claim/release/propose/pass/refute):
/// [`task::require_actor`], with the error naming this surface's `--from` flag.
fn require_from(explicit: Option<String>) -> Result<String> {
    task::require_actor(explicit, "--from")
}

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
    /// Open a new task. Prints its id.
    Open {
        title: String,
        #[arg(long, default_value = "", conflicts_with = "body_file")]
        body: String,
        /// Read the body from a file (`-` for stdin) — a brief is a document, not a shell string.
        #[arg(long = "body-file", value_name = "PATH")]
        body_file: Option<PathBuf>,
        /// Repository this task's code work happens in (enables propose/verify/debt).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// External issue/work handle, free-form.
        #[arg(long)]
        issue: Option<String>,
        /// Board channel task notifications post to.
        #[arg(long, default_value = "tasks")]
        channel: String,
        #[arg(long)]
        assignee: Option<String>,
        /// Comma-separated tags (`decision,deploy`); `list --tag` filters on them and a `decision`
        /// tag on an assigned task puts it in the assignee's inbox under "decisions owed".
        #[arg(long, value_name = "A,B")]
        tags: Option<String>,
        /// This task waits on another (id or unique prefix). Repeatable. Blocked-ness is computed
        /// from the blocker's live state — it clears by itself when the blocker finishes.
        #[arg(long = "blocked-by", value_name = "ID")]
        blocked_by: Vec<String>,
        /// Another task waits on this one (writes a `blocked_by` on THAT task). Repeatable.
        #[arg(long = "blocks", value_name = "ID")]
        blocks: Vec<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// List tasks (non-terminal by default).
    List {
        /// Filter by effective state (open|claimed|awaiting_review|ready|merged_local|landed|done|...).
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Only tasks carrying this tag.
        #[arg(long)]
        tag: Option<String>,
        /// Include terminal tasks.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        /// Tab-separated rows for scripts: full id, state, assignee, repo basename, age, title,
        /// blocked_by — no alignment, no truncation.
        #[arg(long, conflicts_with = "json")]
        tsv: bool,
        /// Two lines per task: the row, then tags · repo · blockers · the body's first line.
        #[arg(long, conflicts_with_all = ["json", "tsv"])]
        wide: bool,
    },
    /// Show one task (id may be a unique prefix).
    Show {
        id: String,
        #[arg(long)]
        json: bool,
        /// Also print the raw event history.
        #[arg(long)]
        events: bool,
    },
    /// Claim an open task (first writer wins).
    Claim {
        id: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        /// TOFU per-endpoint token authenticating the `--from` claim. Default: $CV_TOKEN. First use
        /// binds the endpoint to this token; thereafter it is required.
        #[arg(long)]
        token: Option<String>,
    },
    /// Release a claim back to open.
    Release {
        id: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        /// TOFU per-endpoint token authenticating the `--from` claim. Default: $CV_TOKEN.
        #[arg(long)]
        token: Option<String>,
    },
    /// Record a progress note.
    Note {
        id: String,
        /// The note. Omit it and pass `--file` to read the note from a file.
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        text: Option<String>,
        /// Read the note from a file (`-` for stdin).
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Complete a non-code task (refused while a code revision is live — land or kill it first).
    ///
    /// With a `--check-*`, cv RUNS the check and records the result: a pass makes the completion
    /// OBSERVED (provenance `checked`), a failure REFUSES the done (the task stays open). At most
    /// one check kind may be given; a checkless done is self-reported, the honest fallback.
    Done {
        id: String,
        /// Pointer to observable evidence (URL, path, session id).
        #[arg(long)]
        observed: Option<String>,
        /// Run this shell command; exit 0 = pass, nonzero refuses the done (runs in the task's repo
        /// dir if it has one, else cwd).
        #[arg(long = "check-cmd", group = "check")]
        check_cmd: Option<String>,
        /// Require this path to exist and be non-empty (relative paths resolve in the task's repo).
        #[arg(long = "check-file", group = "check")]
        check_file: Option<PathBuf>,
        /// GET this http:// url; a 2xx = pass. (https is not built in — use --check-cmd 'curl -fsS …'.)
        #[arg(long = "check-http", group = "check")]
        check_http: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Kill a task.
    Abandon {
        id: String,
        #[arg(long, default_value = "no reason given")]
        reason: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Mark a task superseded by another.
    Supersede {
        id: String,
        #[arg(long = "by")]
        by_task: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Add tags to a task (comma-separated; additive, never removes).
    Tag {
        id: String,
        #[arg(value_name = "A,B")]
        tags: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Record that a task waits on another: `cv task block <id> --by <blocker>`.
    Block {
        id: String,
        /// The task that must finish first (id or unique prefix).
        #[arg(long = "by", value_name = "ID")]
        by_task: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Candidates for closing, from what git and the filesystem say — never closes anything.
    /// Lists non-terminal tasks whose `--issue` path no longer exists, or whose title/body names a
    /// branch now merged into the repo's main (`git branch --merged`).
    Sweep {
        /// The repository to observe (branches are read from it; a task's own `--repo` must
        /// match, or be unset).
        #[arg(long)]
        repo: PathBuf,
        /// The branch merged work lands on (default: `main`, else `master` if that is what exists).
        #[arg(long)]
        main: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Propose a reviewed revision: cv resolves the branch tip and computes the range patch-id
    /// from git itself (identity is observed, never typed).
    Propose {
        id: String,
        #[arg(long)]
        branch: String,
        /// Verify this sha is the branch tip (refused otherwise). Default: use the tip.
        #[arg(long)]
        sha: Option<String>,
        #[arg(long, default_value = "origin/main")]
        upstream: String,
        #[arg(long)]
        worktree: Option<PathBuf>,
        /// Reviewer endpoint bound to this revision (else the first verdict binds).
        #[arg(long)]
        reviewer: Option<String>,
        /// Your cv session id (author side of the independence check).
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        /// TOFU per-endpoint token authenticating the `--from` claim. Default: $CV_TOKEN.
        #[arg(long)]
        token: Option<String>,
    },
    /// Reroute the active review to another reviewer.
    Reroute {
        id: String,
        #[arg(long)]
        to: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Record a review PASS (advisory independence check runs if --session is given).
    Pass {
        id: String,
        /// The reviewer's cv session id — used to read the reviewer's harness family.
        #[arg(long)]
        session: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        /// TOFU per-endpoint token authenticating the `--from` claim. Default: $CV_TOKEN.
        #[arg(long)]
        token: Option<String>,
    },
    /// Record a review REFUTE (terminal for the revision; propose a new revision to continue).
    Refute {
        id: String,
        #[arg(long)]
        session: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        /// TOFU per-endpoint token authenticating the `--from` claim. Default: $CV_TOKEN.
        #[arg(long)]
        token: Option<String>,
    },
    /// Run the git verifier over Ready/MergedLocal revisions and record what it observes.
    Verify {
        /// Task id (prefix ok). Omit with --all to verify everything verifiable.
        id: Option<String>,
        #[arg(long)]
        all: bool,
        /// git fetch the upstream's remote first.
        #[arg(long)]
        fetch: bool,
        /// Skip re-observation of Landed revisions (opt-out for huge histories; previously
        /// recorded suspects are preserved, not cleared).
        #[arg(long = "skip-landed")]
        skip_landed: bool,
    },
    /// What needs `<who>`: assigned/claimed tasks, reviews awaiting them, their unlanded work.
    Inbox {
        /// Endpoint to compute the inbox for. Default: $CV_ENDPOINT, else "cv".
        who: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reviewed-but-unlanded work, grouped by repo, oldest first. The honest debt view.
    Debt {
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Fleet batting averages: per-endpoint and per-reviewer outcome counts, computed from
    /// observed events only. Informational — never a gate.
    Stats {
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

/// Replay the default store, print any log warnings loudly, return the outcome.
fn replay_loud() -> Result<cv_core::task::ReplayOutcome> {
    let outcome = task::replay()?;
    for w in &outcome.warnings {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    Ok(outcome)
}

/// Append an agent event through the shared core path, print a one-line confirmation. `token` is
/// the TOFU credential presented on identity-bearing appends (`--token`, else `$CV_TOKEN`); it is
/// inert for bookkeeping verbs and for endpoints that have never bound a token.
fn append_and_report(task_id: Option<&str>, from: &str, kind: TaskEventKind, token: Option<String>) -> Result<()> {
    append_event(task_id, from, kind, token).map(|_| ())
}

/// [`append_and_report`], returning the event's task id (what `open` needs to attach its tags
/// and relations to the task it just created).
fn append_event(task_id: Option<&str>, from: &str, kind: TaskEventKind, token: Option<String>) -> Result<String> {
    let store = TaskStore::default_store().with_token(task::token(token));
    let report = task::append_and_notify(&store, task_id, from, kind, Vec::new())?;
    for w in report.replay_warnings.iter().chain(&report.warnings) {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    let state = report.effective_state.unwrap_or_else(|| "?".into());
    println!(
        "✦ {} {} → {}",
        report.event.kind.tag(),
        prefix(&report.event.task_id, 13),
        state
    );
    if matches!(report.event.kind, TaskEventKind::Opened { .. }) {
        println!("{}", report.event.task_id);
    }
    Ok(report.event.task_id)
}

fn resolve<'m>(model: &'m cv_core::task::TaskReadModel, prefix: &str) -> Result<&'m str> {
    task::resolve_id(model, prefix).map_err(|e| anyhow::anyhow!(e))
}

/// One `cv task list` row: id, age since last event (G8), state, assignee, title — rendered from
/// the shared [`TaskRow`] shape (built via [`TaskRow::full`], whose `last_ts` anchors the age).
/// All free-text fields are terminal-sanitized (G5) — the log is forever, so every reader strips.
fn task_row(r: &TaskRow, now: DateTime<Utc>, plen: usize, blocked: bool) -> String {
    let assignee = r.assignee.as_deref().unwrap_or("-");
    format!(
        "{:<plen$}  {:>4}  {:16} {:20} {}{}",
        prefix(&r.id, plen),
        task::age_short(r.last_ts.unwrap_or(now), now),
        r.effective_state,
        sanitize_line(assignee),
        if blocked { "⊘ " } else { "" },
        sanitize_line(&r.title)
    )
}

/// `--tsv`: full id, state, assignee, repo basename, age, title, blocked_by — one tab between
/// fields, tabs/newlines inside a field replaced, nothing aligned or cut. For `cut`/`awk`/`sort`.
fn task_row_tsv(r: &TaskRow, now: DateTime<Utc>) -> String {
    let cell = |s: &str| sanitize_line(s).replace(['\t', '\n'], " ");
    let repo = r
        .repo
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    [
        r.id.clone(),
        r.effective_state.clone(),
        cell(r.assignee.as_deref().unwrap_or("")),
        cell(&repo),
        task::age_short(r.last_ts.unwrap_or(now), now),
        cell(&r.title),
        r.blocked_by.join(","),
    ]
    .join("\t")
}

/// `--wide`: the row, then a second line with tags · repo · blockers · the body's first line.
fn task_row_wide(t: &TaskProjection, model: &TaskReadModel, now: DateTime<Utc>, plen: usize) -> String {
    let mut out = task_row(&TaskRow::full(t), now, plen, task::is_blocked(model, t));
    let mut facts: Vec<String> = Vec::new();
    if !t.tags.is_empty() {
        facts.push(format!("#{}", t.tags.join(" #")));
    }
    if let Some(repo) = &t.repo {
        facts.push(
            repo.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
    }
    if !t.blocked_by.is_empty() {
        let names: Vec<String> = t
            .blocked_by
            .iter()
            .map(|b| match model.tasks.get(b) {
                Some(bt) => format!("{} [{}]", prefix(b, plen), task::effective_display(bt)),
                None => format!("{} [unknown]", prefix(b, plen)),
            })
            .collect();
        facts.push(format!("blocked by {}", names.join(", ")));
    }
    let body = t.body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if !body.is_empty() {
        facts.push(truncate(&sanitize_line(body), 160));
    }
    if !facts.is_empty() {
        out.push_str("\n    ");
        out.push_str(&sanitize_line(&facts.join(" · ")));
    }
    out
}

/// One `cv task inbox` row, stalest first upstream: a `⏰` leads anything waiting > 24h. A
/// projection of age, not an escalation — the marker is the whole mechanism.
fn inbox_row(r: &InboxRow, now: DateTime<Utc>, plen: usize) -> String {
    let stale = now.signed_duration_since(r.since) > chrono::Duration::hours(24);
    format!(
        "{} {:<plen$}  {:>4}  {:16} {}",
        if stale { "⏰" } else { "  " },
        prefix(&r.id, plen),
        task::age_short(r.since, now),
        r.effective_state,
        sanitize_line(&r.title)
    )
}

/// The inbox's groups, in the order they are printed: a decision owed is the slowest blocker a
/// fleet has, so it comes first; then the work that is yours; then what others wait on you for.
const INBOX_GROUPS: &[(InboxReason, &str)] = &[
    (InboxReason::DecisionOwed, "decisions owed"),
    (InboxReason::ClaimedByYou, "claimed"),
    (InboxReason::AwaitingYourReview, "reviews"),
    (InboxReason::YourUnlandedWork, "unlanded"),
    (InboxReason::AssignedOpen, "assigned, unclaimed"),
];

pub(crate) fn cmd_task(action: TaskCmd) -> Result<()> {
    match action {
        TaskCmd::Open {
            title,
            body,
            body_file,
            repo,
            issue,
            channel,
            assignee,
            tags,
            blocked_by,
            blocks,
            from,
        } => {
            let repo = match repo {
                Some(r) => Some(
                    r.canonicalize()
                        .with_context(|| format!("repo {} not found", r.display()))?,
                ),
                None => None,
            };
            let body = match &body_file {
                Some(p) => read_text_arg(p)?,
                None => body,
            };
            let tags = tags.as_deref().map(parse_tags).unwrap_or_default();
            // Relations name other tasks: resolve every prefix BEFORE the open, so a typo refuses
            // the whole command instead of leaving a task opened with half its relations.
            let (blocked_by, blocks) = {
                let outcome = replay_loud()?;
                let res = |ids: Vec<String>| -> Result<Vec<String>> {
                    ids.iter()
                        .map(|p| resolve(&outcome.model, p).map(str::to_string))
                        .collect()
                };
                (res(blocked_by)?, res(blocks)?)
            };
            let from = from_or_cv(from);
            let id = append_event(
                None,
                &from,
                TaskEventKind::Opened {
                    title,
                    body,
                    repo,
                    issue,
                    channel,
                    assignee,
                },
                None,
            )?;
            if !tags.is_empty() {
                append_and_report(Some(&id), &from, TaskEventKind::Tagged { tags }, None)?;
            }
            for b in blocked_by {
                append_and_report(Some(&id), &from, TaskEventKind::BlockedBy { task: b }, None)?;
            }
            for other in blocks {
                append_and_report(Some(&other), &from, TaskEventKind::BlockedBy { task: id.clone() }, None)?;
            }
            Ok(())
        }
        TaskCmd::List {
            state,
            assignee,
            repo,
            tag,
            all,
            json,
            tsv,
            wide,
        } => {
            let outcome = replay_loud()?;
            let filter = task::TaskFilter {
                state,
                assignee,
                repo,
                include_terminal: all,
                tag,
            };
            let tasks = task::list(&outcome.model, &filter).map_err(|e| anyhow::anyhow!(e))?;
            if json {
                // The full projections, unchanged wire shape (`show --json` sibling).
                println!("{}", serde_json::to_string_pretty(&tasks)?);
                return Ok(());
            }
            let now = Utc::now();
            if tsv {
                for t in &tasks {
                    println!("{}", task_row_tsv(&TaskRow::full(t), now));
                }
                return Ok(());
            }
            let plen = prefix_len(&outcome.model);
            for t in &tasks {
                if wide {
                    println!("{}", task_row_wide(t, &outcome.model, now, plen));
                } else {
                    println!(
                        "{}",
                        task_row(&TaskRow::full(t), now, plen, task::is_blocked(&outcome.model, t))
                    );
                }
            }
            if tasks.is_empty() {
                println!("(no matching tasks)");
            } else if tasks.iter().any(|t| task::is_blocked(&outcome.model, t)) {
                println!("⊘ = blocked by a task that has not finished");
            }
            Ok(())
        }
        TaskCmd::Show { id, json, events } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            if json {
                println!("{}", serde_json::to_string_pretty(t)?);
            } else {
                // Every free-text field below came from the durable log — sanitize at render
                // (G5): titles, notes, endpoints, and branch names are all untrusted.
                println!("task {}  [{}]", t.task_id, task::effective_display(t));
                // Provenance for the terminal facts: a self-reported completion is labeled as
                // such (never silently equal to a verified land), and landing carries the
                // verifier's freshness read.
                let hb = cv_core::task::verify::read_heartbeat(&task::tasks_dir());
                let hb_ts = hb.as_ref().map(|h| h.ts);
                let hb_interval = hb.as_ref().and_then(|h| h.interval_secs);
                let show_now = Utc::now();
                if t.state == cv_core::task::TaskState::Done {
                    // A checked completion cv observed itself (provenance `checked`) renders
                    // distinctly from a self-reported one — law 1 extended to non-code tasks.
                    let (prov, detail) = match &t.done_check {
                        Some(dc) => (
                            cv_core::task::Provenance::checked(Some(t.last_ts)),
                            format!(" ({})", sanitize_line(&describe_check(dc))),
                        ),
                        None => (
                            cv_core::task::Provenance::self_reported(Some(t.last_ts)),
                            t.done_observed
                                .as_deref()
                                .map(|o| format!(" ({})", sanitize_line(o)))
                                .unwrap_or_default(),
                        ),
                    };
                    println!("  done:     {}{}", freshness_phrase(&prov, show_now), detail);
                }
                println!("  title:    {}", sanitize_line(&t.title));
                if !t.body.is_empty() {
                    println!("  body:     {}", sanitize_line(&t.body));
                }
                if let Some(repo) = &t.repo {
                    println!("  repo:     {}", repo.display());
                }
                if let Some(issue) = &t.issue {
                    println!("  issue:    {}", sanitize_line(issue));
                }
                println!("  channel:  #{}", sanitize_line(&t.channel));
                println!("  assignee: {}", sanitize_line(t.assignee.as_deref().unwrap_or("-")));
                if !t.tags.is_empty() {
                    println!("  tags:     {}", sanitize_line(&t.tags.join(", ")));
                }
                let plen = prefix_len(&outcome.model);
                for b in &t.blocked_by {
                    match outcome.model.tasks.get(b) {
                        Some(bt) => println!(
                            "  blocked by: {} [{}] {}",
                            prefix(b, plen),
                            task::effective_display(bt),
                            sanitize_line(&bt.title)
                        ),
                        None => println!("  blocked by: {} [unknown task]", prefix(b, plen)),
                    }
                }
                if task::is_blocked(&outcome.model, t) {
                    println!("  ⊘ BLOCKED — a blocker above has not finished");
                }
                for other in task::blocks(&outcome.model, &t.task_id) {
                    println!(
                        "  blocks:   {} [{}] {}",
                        prefix(&other.task_id, plen),
                        task::effective_display(other),
                        sanitize_line(&other.title)
                    );
                }
                println!(
                    "  opened:   {} by {}",
                    fmt_local(t.opened_at, "%Y-%m-%d %H:%M"),
                    sanitize_line(&t.opened_by)
                );
                for rev in &t.revisions {
                    println!(
                        "  rev {}: {} [{}] {} → {}",
                        rev.revision.n,
                        sanitize_line(&rev.revision.branch),
                        rev.state.as_str(),
                        &rev.revision.review_sha[..12],
                        sanitize_line(&rev.revision.upstream),
                    );
                    if let Some(reviewer) = &rev.active_reviewer {
                        println!("         reviewer: {}", sanitize_line(reviewer));
                    }
                    for (verdict, receipts) in [
                        ("pass", rev.pass.as_ref().and_then(|p| p.receipts.as_ref())),
                        ("refute", rev.refute.as_ref().and_then(|r| r.receipts.as_ref())),
                    ] {
                        if let Some(r) = receipts {
                            println!("         receipts ({verdict}): {}", sanitize_line(&receipts_line(r)));
                        }
                    }
                    if let Some((head, pid)) = &rev.landed {
                        // The land itself was git-observed (that is where landed_at comes from);
                        // freshness only qualifies whether the latest pass has re-confirmed it.
                        let observed = rev
                            .landed_at
                            .map(|at| format!("observed {} ago", task::age_short(at, show_now)))
                            .unwrap_or_else(|| "observation time unknown".into());
                        let qualifier = match rev.landed_at.map(|at| {
                            cv_core::task::Provenance::git_verified(at, hb_ts, hb_interval, show_now).freshness
                        }) {
                            Some(cv_core::task::Freshness::Stale { .. }) => " (verifier stale)",
                            Some(cv_core::task::Freshness::Unknown) => " (unconfirmed by latest pass)",
                            _ => "",
                        };
                        println!(
                            "         landed: upstream {} (patch-id {}) · git-verified, {}{}",
                            &head[..12],
                            &pid[..12],
                            observed,
                            qualifier
                        );
                    }
                    for issue in &rev.issues {
                        println!("         ⚠ {}", sanitize_line(&issue.describe()));
                    }
                }
                for note in &t.notes {
                    println!(
                        "  note ({}, {}): {}",
                        sanitize_line(&note.by),
                        fmt_local(note.ts, "%Y-%m-%d %H:%M"),
                        sanitize_line(&note.text)
                    );
                }
            }
            if events {
                let history: Vec<_> = outcome.events.iter().filter(|e| e.task_id == id).collect();
                println!("{}", serde_json::to_string_pretty(&history)?);
            }
            Ok(())
        }
        TaskCmd::Claim { id, from, token } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::Claimed { assignee: from.clone() },
                token,
            )
        }
        TaskCmd::Release { id, from, token } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Released {}, token)
        }
        TaskCmd::Note {
            id,
            text,
            file,
            session_ref,
            from,
        } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let text = match (text, file) {
                (Some(t), _) => t,
                (None, Some(f)) => read_text_arg(&f)?,
                (None, None) => unreachable!("clap requires text or --file"),
            };
            append_and_report(
                Some(&id),
                &from_or_cv(from),
                TaskEventKind::Noted { text, session_ref },
                None,
            )
        }
        TaskCmd::Done {
            id,
            observed,
            check_cmd,
            check_file,
            check_http,
            from,
        } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            // If a check was requested, cv RUNS it now. A pass records HOW the completion was
            // observed; a failure refuses the done here (nothing is appended — the task stays open).
            let spec = check_cmd
                .map(cv_core::task::CheckSpec::Cmd)
                .or_else(|| check_file.map(cv_core::task::CheckSpec::File))
                .or_else(|| check_http.map(cv_core::task::CheckSpec::Http));
            let (observed, check) = match spec {
                Some(spec) => {
                    let done_check = spec.run(t.repo.as_deref())?;
                    // Default the human `observed` pointer to the check's own result when the caller
                    // gave none, so `task show` always has something to print next to `checked`.
                    let observed = observed.or_else(|| Some(done_check.result.clone()));
                    println!("✓ completion check passed: {}", sanitize_line(&done_check.result));
                    (observed, Some(done_check))
                }
                None => (observed, None),
            };
            append_and_report(
                Some(&id),
                &from_or_cv(from),
                TaskEventKind::Done { observed, check },
                None,
            )
        }
        TaskCmd::Abandon { id, reason, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Abandoned { reason }, None)
        }
        TaskCmd::Supersede { id, by_task, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let by_task = resolve(&outcome.model, &by_task)?.to_string();
            append_and_report(
                Some(&id),
                &from_or_cv(from),
                TaskEventKind::Superseded { by_task },
                None,
            )
        }
        TaskCmd::Tag { id, tags, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let tags = parse_tags(&tags);
            if tags.is_empty() {
                bail!("no tags given (comma-separated, e.g. `decision,deploy`)");
            }
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Tagged { tags }, None)
        }
        TaskCmd::Block { id, by_task, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let by_task = resolve(&outcome.model, &by_task)?.to_string();
            append_and_report(
                Some(&id),
                &from_or_cv(from),
                TaskEventKind::BlockedBy { task: by_task },
                None,
            )
        }
        TaskCmd::Sweep { repo, main, json } => cmd_sweep(&repo, main, json),
        TaskCmd::Propose {
            id,
            branch,
            sha,
            upstream,
            worktree,
            reviewer,
            session_ref,
            from,
            token,
        } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            let repo = t
                .repo
                .clone()
                .context("task has no --repo recorded; open it with one to propose revisions")?;
            let n = t.revisions.len() as u32 + 1;
            let revision = cv_core::task::verify::observe_revision(
                &repo,
                &branch,
                &upstream,
                sha.as_deref(),
                n,
                worktree,
                reviewer,
                session_ref,
            )?;
            println!(
                "observed: {} tip {} range-patch-id {}",
                sanitize_line(&branch),
                &revision.review_sha[..12],
                &revision.patch_id[..12]
            );
            // Advisory collision scan (never a block): another live task already carrying this
            // branch/worktree in the same repo is usually two agents about to trample each other.
            for w in task::propose_collision_warnings(&outcome.model, &id, &repo, &revision) {
                eprintln!("⚠ WARNING: {}", sanitize_line(&w));
            }
            append_and_report(Some(&id), &from, TaskEventKind::RevisionProposed { revision }, token)
        }
        TaskCmd::Reroute { id, to, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let current = outcome.model.tasks[&id]
                .current_revision()
                .and_then(|r| r.active_reviewer.clone())
                .context("no active reviewer to reroute from")?;
            append_and_report(
                Some(&id),
                &from_or_cv(from),
                TaskEventKind::ReviewRerouted { from: current, to },
                None,
            )
        }
        TaskCmd::Pass {
            id,
            session,
            from,
            token,
        } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            // Both advisory observations from one parse of the reviewer's session (law 2:
            // read, record, warn — never gate).
            let obs = task::review_observation(&outcome.model.tasks[&id], session.as_deref());
            if let Some(w) = task::independence_warning(obs.independence.as_ref()) {
                eprintln!("⚠ {w}");
            }
            if let Some(w) = task::receipts_warning(obs.receipts.as_ref()) {
                eprintln!("⚠ {w}");
            }
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewPassed {
                    reviewer: from.clone(),
                    session_ref: session,
                    independence: obs.independence,
                    receipts: obs.receipts,
                },
                token,
            )
        }
        TaskCmd::Refute {
            id,
            session,
            from,
            token,
        } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let receipts = task::review_receipts(&outcome.model.tasks[&id], session.as_deref());
            if let Some(w) = task::receipts_warning(receipts.as_ref()) {
                eprintln!("⚠ {w}");
            }
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewRefuted {
                    reviewer: from.clone(),
                    session_ref: session,
                    receipts,
                },
                token,
            )
        }
        TaskCmd::Verify {
            id,
            all,
            fetch,
            skip_landed,
        } => cmd_verify(id, all, fetch, skip_landed),
        TaskCmd::Inbox { who, json } => {
            // Bare `cv task inbox` means "my inbox": the spawner-set CV_ENDPOINT, else "cv".
            let who = from_or_cv(who);
            let outcome = replay_loud()?;
            if json {
                // The full entries, unchanged wire shape.
                let entries = task::inbox(&outcome.model, &who);
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                let rows = InboxRow::compute(&outcome.model, &who);
                let now = Utc::now();
                let plen = prefix_len(&outcome.model);
                if rows.is_empty() {
                    println!("(inbox empty for {})", sanitize_line(&who));
                }
                // Grouped, stalest first within a group (the rows arrive stalest first overall).
                for (reason, label) in INBOX_GROUPS {
                    let group: Vec<&InboxRow> = rows.iter().filter(|r| r.reason == *reason).collect();
                    if group.is_empty() {
                        continue;
                    }
                    println!("{label} ({}):", group.len());
                    for r in group {
                        println!("{}", inbox_row(r, now, plen));
                    }
                }
            }
            Ok(())
        }
        TaskCmd::Debt { repo, json } => {
            let outcome = replay_loud()?;
            // The debt view is only as honest as the verifier is alive (G1) — the heartbeat
            // rides it, and suspect lands (G3) re-surface as visible debt. Shaping (repo filter,
            // aged awaiting-review rows (G8), warning text) is the shared DebtReport.
            let hb = cv_core::task::verify::read_heartbeat(&task::tasks_dir());
            if json {
                // The full entries (task projection embedded), plus an additive `provenance` key
                // per row deriving landing freshness from the heartbeat — every unlanded fact says
                // how fresh the verifier's read of it is (Unknown when never checked).
                let hb_ts = hb.as_ref().map(|h| h.ts);
                let hb_interval = hb.as_ref().and_then(|h| h.interval_secs);
                let now = Utc::now();
                let json_rows: Vec<_> = task::debt(&outcome.model)
                    .iter()
                    .filter(|(group_repo, _)| match &repo {
                        Some(want) => group_repo.as_deref() == Some(want.as_path()),
                        None => true,
                    })
                    .flat_map(|(_, entries)| entries.clone())
                    .map(|e| {
                        let prov = cv_core::task::Provenance::awaiting_land(e.since, hb_ts, hb_interval, now);
                        let mut v = serde_json::to_value(&e).expect("DebtEntry serializes");
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "provenance".into(),
                                serde_json::to_value(prov).expect("Provenance serializes"),
                            );
                        }
                        v
                    })
                    .collect();
                let awaiting: Vec<_> = task::awaiting_review(&outcome.model)
                    .into_iter()
                    .filter(|e| match &repo {
                        Some(want) => e.task.repo.as_deref() == Some(want.as_path()),
                        None => true,
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "debt": json_rows,
                        "awaiting_review": awaiting,
                        "suspects": hb.as_ref().map(|h| h.suspect_landed.clone()).unwrap_or_default(),
                        "verified_as_of": hb.as_ref().map(|h| h.ts),
                        "verify_warning": cv_core::task::verify::heartbeat_warning(hb.as_ref()),
                    }))?
                );
                return Ok(());
            }
            let report = task::DebtReport::compute(&outcome.model, hb.as_ref(), repo.as_deref());
            let plen = prefix_len(&outcome.model);
            // Rows arrive repo-ascending (no-repo first), oldest first within a repo: render a
            // group header at each repo transition.
            let mut current: Option<&Option<std::path::PathBuf>> = None;
            let now = Utc::now();
            for row in &report.debt {
                if current != Some(&row.repo) {
                    let name = row
                        .repo
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(no repo)".into());
                    println!("{name}:");
                    current = Some(&row.repo);
                }
                let age = now.signed_duration_since(row.since);
                println!(
                    "  {}  rev{} {} [{}] unlanded for {}h · {} — {}",
                    prefix(&row.id, plen),
                    row.revision,
                    sanitize_line(&row.branch),
                    row.state.as_str(),
                    age.num_hours(),
                    freshness_phrase(&row.provenance, now),
                    sanitize_line(&row.title)
                );
                for issue in &row.issues {
                    println!("      ⚠ {}", sanitize_line(issue));
                }
            }
            if report.debt.is_empty() && report.suspects.is_empty() {
                println!("✓ no unlanded reviewed work");
            }
            if !report.awaiting_review.is_empty() {
                println!("awaiting review:");
                for row in &report.awaiting_review {
                    println!(
                        "  {}  rev{} {} → {} waiting {} — {}",
                        prefix(&row.id, plen),
                        row.revision,
                        sanitize_line(&row.branch),
                        sanitize_line(row.reviewer.as_deref().unwrap_or("(reviewer unbound)")),
                        task::age_short(row.since, now),
                        sanitize_line(&row.title)
                    );
                }
            }
            // Suspects were all observed by the current pass — date their freshness off the
            // heartbeat itself (the debt view is only as fresh as its verifier).
            let suspect_prov = hb
                .as_ref()
                .map(|h| cv_core::task::Provenance::git_verified(h.ts, Some(h.ts), h.interval_secs, now));
            for s in &report.suspects {
                let fresh = suspect_prov
                    .as_ref()
                    .map(|p| format!(" · {}", freshness_phrase(p, now)))
                    .unwrap_or_default();
                println!(
                    "⚠ SUSPECT {}  rev{} {}{} — {}",
                    prefix(&s.task_id, plen),
                    s.revision,
                    sanitize_line(&s.detail),
                    fresh,
                    sanitize_line(&s.title)
                );
            }
            if let Some(hb) = &hb {
                println!(
                    "verified as of {}{}",
                    fmt_local(hb.ts, "%Y-%m-%d %H:%M:%S"),
                    hb.interval_secs.map(|i| format!(" (every {i}s)")).unwrap_or_default()
                );
            }
            if let Some(w) = &report.verify_warning {
                eprintln!("⚠ {}", sanitize_line(w));
            }
            Ok(())
        }
        TaskCmd::Stats { repo, json } => {
            let outcome = replay_loud()?;
            let hb = cv_core::task::verify::read_heartbeat(&task::tasks_dir());
            let stats = task::FleetStats::compute(&outcome.model, hb.as_ref(), repo.as_deref());
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                render_stats(&stats);
            }
            Ok(())
        }
    }
}

/// A compact human phrase for a fact's freshness — how it was learned and when. Reused across the
/// debt and show surfaces so provenance reads the same everywhere: `observed 4m ago` /
/// `last checked 2h ago (stale)` / `NEVER verified` / `self-reported`.
fn freshness_phrase(p: &cv_core::task::Provenance, now: DateTime<Utc>) -> String {
    use cv_core::task::Freshness;
    match p.freshness {
        Freshness::Unknown if p.source == cv_core::task::provenance::SOURCE_SELF_REPORT => "self-reported".into(),
        Freshness::Unknown => "NEVER verified".into(),
        // A checked completion: cv ran the predicate itself — say "checked" rather than the generic
        // "observed", so the surface distinguishes it from a git-verified land at a glance.
        Freshness::Fresh if p.source == cv_core::task::provenance::SOURCE_CHECKED => match p.observed_at {
            Some(t) => format!("checked {} ago", task::age_short(t, now)),
            None => "checked".into(),
        },
        Freshness::Fresh => match p.observed_at {
            Some(t) => format!("observed {} ago", task::age_short(t, now)),
            None => "verified".into(),
        },
        Freshness::Stale { .. } => match p.observed_at {
            Some(t) => format!("last checked {} ago (stale)", task::age_short(t, now)),
            None => "stale".into(),
        },
    }
}

/// A completion check as a human line: `cmd: cargo test → exit 0` / `file: design.md → exists, 42
/// bytes` / `http: http://… → 200 OK`. The target itself is untrusted log content, sanitized by
/// the caller.
fn describe_check(dc: &cv_core::task::DoneCheck) -> String {
    use cv_core::task::DoneCheckKind;
    let (label, target) = match &dc.kind {
        DoneCheckKind::Cmd { cmd } => ("cmd", cmd.clone()),
        DoneCheckKind::File { path } => ("file", path.display().to_string()),
        DoneCheckKind::Http { url } => ("http", url.clone()),
    };
    format!("{label}: {target} → {}", dc.result)
}

/// One receipts observation as a human line: `saw change ✓, ran checks ✗, 14 turns`
/// (`?` = undetermined — observed as unknown, never guessed).
fn receipts_line(r: &cv_core::task::ReviewReceipts) -> String {
    let mark = |o: Option<bool>| match o {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "?",
    };
    let turns = r
        .turns
        .map(|t| format!("{t} turns"))
        .unwrap_or_else(|| "? turns".into());
    format!(
        "saw change {}, ran checks {}, {}",
        mark(r.saw_change),
        mark(r.ran_checks),
        turns
    )
}

/// A median duration cell: `-` when there is no sample (never a fake `0s`).
fn median_cell(secs: Option<i64>) -> String {
    let now = Utc::now();
    match secs {
        None => "-".into(),
        Some(s) => task::age_short(now - chrono::Duration::seconds(s.max(0)), now),
    }
}

/// `cv task stats` human rendering. Small-n honesty: rates print as the counts they came from
/// (`2/3`), never a bare percentage.
fn render_stats(stats: &cv_core::task::FleetStats) {
    if stats.endpoints.is_empty() && stats.reviewers.is_empty() {
        println!("(no task history yet)");
    }
    if !stats.endpoints.is_empty() {
        println!("endpoints (as author/assignee):");
        println!(
            "  {:24} {:>7} {:>8} {:>7} {:>7} {:>5} {:>6}  {:>9} {:>9}",
            "endpoint", "claimed", "proposed", "landed", "refuted", "live", "aband", "land-rate", "med-land"
        );
        for e in &stats.endpoints {
            let terminal = e.landed + e.refuted + e.superseded;
            let rate = if terminal == 0 {
                "-".into()
            } else {
                format!("{}/{terminal}", e.landed)
            };
            println!(
                "  {:24} {:>7} {:>8} {:>7} {:>7} {:>5} {:>6}  {:>9} {:>9}",
                sanitize_line(&e.endpoint),
                e.claimed,
                e.proposed,
                e.landed,
                e.refuted,
                e.unlanded,
                e.abandoned_live,
                rate,
                median_cell(e.median_secs_to_land),
            );
        }
    }
    if !stats.reviewers.is_empty() {
        println!("reviewers:");
        println!(
            "  {:24} {:>8} {:>11} {:>9} {:>8} {:>10}  {:>11}",
            "reviewer", "verdicts", "pass/refute", "same-fam", "no-rcpt", "no-contact", "med-latency"
        );
        for r in &stats.reviewers {
            println!(
                "  {:24} {:>8} {:>11} {:>9} {:>8} {:>10}  {:>11}",
                sanitize_line(&r.reviewer),
                r.verdicts,
                format!("{}/{}", r.passes, r.refutes),
                r.same_family_passes,
                r.no_receipts_passes,
                r.no_contact_passes,
                median_cell(r.median_review_latency_secs),
            );
        }
    }
    for f in &stats.families {
        println!(
            "family {}: {} reviews ({} cross-family, {} same-family, {} undetermined)",
            sanitize_line(&f.family),
            f.reviews_given,
            f.cross_family,
            f.same_family,
            f.undetermined
        );
    }
    match &stats.verified_as_of {
        Some(ts) => println!("verified as of {}", fmt_local(*ts, "%Y-%m-%d %H:%M:%S")),
        None => println!("verified: NEVER"),
    }
    if let Some(w) = &stats.verify_warning {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    println!("computed from observed events only; landed = git-verified");
}

/// `cv task verify` — the observation pass, shared with MCP/cvd via `verify::run_verify`.
fn cmd_verify(id: Option<String>, all: bool, fetch: bool, skip_landed: bool) -> Result<()> {
    if id.is_none() && !all {
        bail!("pass a task id or --all");
    }
    let store = TaskStore::default_store();
    let ids: Option<Vec<String>> = match &id {
        Some(prefix) => {
            let outcome = replay_loud()?;
            Some(vec![resolve(&outcome.model, prefix)?.to_string()])
        }
        None => None,
    };
    let opts = cv_core::task::verify::VerifyOptions {
        fetch,
        skip_landed,
        ..Default::default()
    };
    let (appended, warnings) = cv_core::task::verify::run_verify(&store, ids.as_deref(), &opts)?;
    for w in &warnings {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    for ev in &appended {
        println!("✦ observed {} on {}", ev.kind.tag(), prefix(&ev.task_id, 13));
    }
    if appended.is_empty() {
        println!("(nothing new observed)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::task::{InboxEntry, InboxReason, TaskProjection, TaskState};

    fn proj(title: &str, last_ts: &str) -> TaskProjection {
        TaskProjection {
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            title: title.into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            state: TaskState::Open,
            assignee: Some("agent:a".into()),
            opened_by: "h".into(),
            opened_at: "2026-07-10T00:00:00Z".parse().unwrap(),
            last_event_id: "e".into(),
            last_ts: last_ts.parse().unwrap(),
            revisions: Vec::new(),
            notes: Vec::new(),
            superseded_by: None,
            abandoned_reason: None,
            done_observed: None,
            done_check: None,
            tags: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn task_row_shows_age_and_strips_ansi() {
        let t = proj("evil\u{1b}]0;pwn\u{7}title\u{1b}[31m!", "2026-07-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let row = task_row(&TaskRow::full(&t), now, 8, false);
        assert!(row.contains("  3d  "), "age column from last_ts: {row}");
        assert!(row.contains("eviltitle!"), "payload stripped: {row}");
        assert!(!row.contains('\u{1b}'), "no ESC survives: {row}");
    }

    #[test]
    fn inbox_row_marks_older_than_24h_and_strips_ansi() {
        let t = proj("t\u{1b}[31mred", "2026-07-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let stale = InboxRow::from_entry(&InboxEntry {
            task: &t,
            reason: InboxReason::ClaimedByYou,
            since: "2026-07-13T00:00:00Z".parse().unwrap(),
        });
        let row = inbox_row(&stale, now, 8);
        assert!(row.starts_with("⏰"), "24h+ rows lead with the marker: {row}");
        assert!(row.contains("tred") && !row.contains('\u{1b}'), "{row}");

        let fresh = InboxRow::from_entry(&InboxEntry {
            task: &t,
            reason: InboxReason::ClaimedByYou,
            since: "2026-07-15T12:00:00Z".parse().unwrap(),
        });
        let row = inbox_row(&fresh, now, 8);
        assert!(!row.contains('⏰'), "young rows carry no marker: {row}");
        assert!(row.contains("12h"), "{row}");
    }

    #[test]
    fn blocked_rows_carry_the_marker_and_tsv_is_one_line_per_task() {
        let mut t = proj("tab\tin\ntitle", "2026-07-13T00:00:00Z");
        t.blocked_by = vec!["00000000-0000-7000-8000-000000000009".into()];
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let row = task_row(&TaskRow::full(&t), now, 13, true);
        assert!(row.contains("⊘ tab"), "{row}");
        assert!(
            row.starts_with("00000000-0000  "),
            "prefix is sized by the caller: {row}"
        );
        let tsv = task_row_tsv(&TaskRow::full(&t), now);
        assert_eq!(tsv.lines().count(), 1, "{tsv:?}");
        let cells: Vec<&str> = tsv.split('\t').collect();
        assert_eq!(cells.len(), 7, "{cells:?}");
        assert_eq!(cells[0], t.task_id);
        assert_eq!(cells[5], "tab in title");
        assert_eq!(cells[6], "00000000-0000-7000-8000-000000000009");
    }

    #[test]
    fn tags_parse_trimmed_and_deduplicated() {
        assert_eq!(parse_tags(" a, b ,,a,c "), vec!["a", "b", "c"]);
        assert!(parse_tags(", ,").is_empty());
    }

    #[test]
    fn sweep_helpers_tell_branches_from_words_and_paths_from_handles() {
        assert!(branch_like("sdk-ts-repair"));
        assert!(branch_like("k-ran"));
        assert!(branch_like("feature/x"));
        assert!(branch_like("p3b1"));
        assert!(!branch_like("fix"));
        assert!(!branch_like("final"));
        let toks = branch_tokens("one commit on branch `sdk-ts-repair` (see docs/plan.md).");
        assert!(toks.contains("sdk-ts-repair"), "{toks:?}");
        assert!(toks.contains("docs/plan.md"), "{toks:?}");
        assert_eq!(
            issue_path("claudesplosion/planning/SESSION-STATE.md"),
            Some("claudesplosion/planning/SESSION-STATE.md")
        );
        assert_eq!(issue_path("#42"), None);
        assert_eq!(issue_path("https://github.com/x/y/issues/4"), None);
        assert_eq!(issue_path("memory/x.md"), Some("memory/x.md"));
    }

    #[test]
    fn explicit_from_beats_everything_and_missing_identity_names_the_cure() {
        assert_eq!(require_from(Some("agent:me".into())).unwrap(), "agent:me");
        assert_eq!(from_or_cv(Some("agent:me".into())), "agent:me");
        // The unset-env cases are exercised end-to-end in tests/cli.rs (spawned process with a
        // controlled environment — no process-global set_var races here).
    }
}

// ===================== cv task sweep =====================

/// What `git` in `repo` says has landed: every local branch merged into `main`, plus every
/// remote-tracking branch merged into it with its remote prefix stripped — the names a task's
/// body would mention. `main` itself is excluded.
fn merged_branches(repo: &Path, main: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for extra in [&[][..], &["-r"][..]] {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .arg("branch")
            .args(extra)
            .arg("--format=%(refname:short)")
            .arg("--merged")
            .arg(main);
        let o = cmd
            .output()
            .with_context(|| format!("running git branch --merged in {}", repo.display()))?;
        if !o.status.success() {
            bail!(
                "git branch --merged {main} failed in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let name = line.trim();
            if name.is_empty() || name.ends_with("/HEAD") {
                continue;
            }
            let short = name.split_once('/').map(|(_, rest)| rest).unwrap_or(name);
            for n in [name, short] {
                if n != main {
                    out.insert(n.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Does `repo` have a ref named `name`?
fn has_ref(repo: &Path, name: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", name])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A branch name worth matching against prose: it must look like a branch, not a word — a
/// separator or a digit, or eight characters — so a branch called `fix` does not sweep every task
/// whose body says "fix".
fn branch_like(name: &str) -> bool {
    name.contains(['-', '/', '_', '.']) || name.chars().any(|c| c.is_ascii_digit()) || name.len() >= 8
}

/// The branch-shaped tokens of a text (letters, digits and `-_./`), deduplicated.
fn branch_tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.')))
        .map(|t| t.trim_matches(|c| matches!(c, '.' | '/')))
        .filter(|t| t.len() >= 3)
        .map(str::to_string)
        .collect()
}

/// Does `issue` read as a path (as opposed to `#42` or a URL)? A slash or a file extension and no
/// whitespace.
fn issue_path(issue: &str) -> Option<&str> {
    let i = issue.trim();
    let looks = !i.contains(char::is_whitespace)
        && !i.contains("://")
        && (i.contains('/') || Path::new(i).extension().is_some());
    looks.then_some(i)
}

#[derive(serde::Serialize)]
struct SweepRow<'a> {
    task_id: &'a str,
    title: &'a str,
    state: String,
    reasons: Vec<String>,
}

/// `cv task sweep --repo <path>`: the tasks the world says are probably done. Observed from git
/// and the filesystem; prints candidates and never closes one (a human or the agent that owns
/// the task does that, with `done`/`abandon`, having read why).
fn cmd_sweep(repo: &Path, main: Option<String>, json: bool) -> Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("repo {} not found", repo.display()))?;
    let main = match main {
        Some(m) => m,
        None if has_ref(&repo, "main") => "main".into(),
        None if has_ref(&repo, "master") => "master".into(),
        None => bail!(
            "{} has neither `main` nor `master`; pass --main <branch>",
            repo.display()
        ),
    };
    let merged = merged_branches(&repo, &main)?;
    let outcome = replay_loud()?;
    let mut rows: Vec<SweepRow<'_>> = Vec::new();
    for t in outcome.model.tasks.values() {
        if t.state.is_terminal() || t.repo.as_deref().is_some_and(|r| r != repo) {
            continue;
        }
        let mut reasons = Vec::new();
        if let Some(p) = t.issue.as_deref().and_then(issue_path) {
            // A relative issue path was typed from *somewhere*: the task's repo, the swept repo,
            // the directory above it (`~/dev/<other-repo>/…` is the common spelling), or the
            // current directory. It is missing only when none of them has it.
            let p = Path::new(p);
            let bases: Vec<PathBuf> = if p.is_absolute() {
                vec![PathBuf::new()]
            } else {
                let mut b: Vec<PathBuf> = Vec::new();
                b.extend(t.repo.clone());
                b.push(repo.clone());
                b.extend(repo.parent().map(Path::to_path_buf));
                b.extend(std::env::current_dir().ok());
                b.dedup();
                b
            };
            if !bases.iter().any(|b| b.join(p).exists()) {
                let tried: Vec<String> = bases.iter().map(|b| b.join(p).display().to_string()).collect();
                reasons.push(format!("issue path no longer exists ({})", tried.join(", ")));
            }
        }
        let mentioned = branch_tokens(&format!("{}\n{}", t.title, t.body));
        for b in mentioned
            .iter()
            .filter(|b| branch_like(b) && merged.contains(b.as_str()))
        {
            reasons.push(format!("names branch `{b}`, merged into {main}"));
        }
        if let Some(rev) = t.current_revision() {
            if !rev.state.is_terminal() && merged.contains(&rev.revision.branch) {
                reasons.push(format!(
                    "rev{} branch `{}` is merged into {main} (run `cv task verify`)",
                    rev.revision.n, rev.revision.branch
                ));
            }
        }
        if !reasons.is_empty() {
            rows.push(SweepRow {
                task_id: &t.task_id,
                title: &t.title,
                state: task::effective_display(t),
                reasons,
            });
        }
    }
    rows.sort_by(|a, b| a.task_id.cmp(b.task_id));
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "nothing to sweep: no open task names a branch merged into {main} or a missing issue path ({} merged branch(es) observed)",
            merged.len()
        );
        return Ok(());
    }
    let plen = prefix_len(&outcome.model);
    println!(
        "# probably done — {} candidate(s) in {} (observed: {} branch(es) merged into {main}); nothing was closed\n",
        rows.len(),
        repo.display(),
        merged.len()
    );
    for r in &rows {
        println!(
            "{:<plen$}  {:16} {}",
            prefix(r.task_id, plen),
            r.state,
            sanitize_line(r.title)
        );
        for reason in &r.reasons {
            println!("{:plen$}  ↳ {}", "", sanitize_line(reason));
        }
    }
    println!("\nclose what is really done: `cv task done <id> --observed <evidence>` · drop the rest: `cv task abandon <id> --reason …`");
    Ok(())
}
