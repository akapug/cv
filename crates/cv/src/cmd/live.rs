//! `cv scry` — live activity across harnesses (`tail -f` for sessions).

use crate::util::{home_rel, parse_harness, poll_interval, short_id, Wait};
use anyhow::Result;
use cv_core::ir::{truncate, SessionRef};
use cv_core::watch::{EventKind, Filter, SessionEvent, Watcher};

/// `cv scry`'s options, named once at the command boundary.
pub(crate) struct ScryArgs {
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub interval: f64,
    pub existing: bool,
    pub follow: bool,
    pub for_: Option<String>,
}

pub(crate) fn cmd_scry(args: ScryArgs) -> Result<()> {
    let ScryArgs {
        harness,
        cwd,
        interval,
        existing,
        follow,
        for_,
    } = args;
    let interval = poll_interval(interval)?;
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    let wait = Wait::resolve(follow, for_.as_deref())?;
    wait.announce("scry", "live agent activity");
    if matches!(wait, Wait::Once) && !existing {
        // The current state: the sessions a follower would baseline, in the order it polls them.
        for r in Watcher::new(filter, true).discover() {
            println!(
                "{}",
                header("   ·", &r, r.message_count.map_or("–".into(), |n| n.to_string()))
            );
        }
        return Ok(());
    }
    // The baseline read of a large fleet can outlive a short `--for`: it stops at the deadline.
    let mut watcher = Watcher::with_discovery_until(filter, existing, cv_core::discover_all, || wait.expired());
    if !existing && wait.expired() {
        eprintln!(
            "cv scry: --for elapsed during the baseline read, {} sessions in",
            watcher.following()
        );
        return Ok(());
    }
    if !matches!(wait, Wait::Once) {
        eprintln!("cv scry: following {} sessions", watcher.following());
    }
    loop {
        let mut events = Vec::new();
        watcher.poll(cv_core::watch::Poll::default(), |ev| {
            let taken = ev.new_messages.len();
            events.push(ev);
            taken
        });
        for ev in events {
            print_event(&ev);
        }
        if !wait.pause(interval) {
            return Ok(());
        }
    }
}

fn header(tag: &str, r: &SessionRef, messages: String) -> String {
    let where_ = r.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into());
    format!(
        "{tag}  {:8} {:8}  {}  ({messages} msg)",
        r.harness.as_str(),
        short_id(&r.id),
        where_
    )
}

fn print_event(ev: &SessionEvent) {
    let tag = match ev.kind {
        EventKind::New => "✷ new",
        EventKind::Updated => "   +",
    };
    println!("{}", header(tag, &ev.reference, ev.new_messages.len().to_string()));
    for m in &ev.new_messages {
        if let Some(t) = m.text() {
            println!("      {} {}", cv_core::render::role_label(m.role), truncate(&t, 100));
        }
    }
}
