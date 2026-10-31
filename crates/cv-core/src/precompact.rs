//! Precompaction: a session's compaction summary, written before the harness asks for it, and
//! handed to the harness inside its own compaction.
//!
//! A harness compacts when its context window fills: it sends the whole window to a model with
//! its own summary instruction and waits for the answer (Claude Code through a proxy to a Codex
//! model: a median of about 330 s). Most of that window is old and stable, so the summary can be
//! written early, off the critical path, and kept current as the session grows.
//!
//! The handoff runs in four steps (the 0.19 design record, REVIEW-ASTRA §3):
//!
//! 1. **prepare**: one owner per context generation takes a lease ([`store::take_lease`]) and
//!    cuts the window at its newest message;
//! 2. **ready**: the summary is published whole as a [`Record`], keyed by a cv checkpoint of the
//!    transcript plus the harness's own id of the cut message ([`Key`]), never by a hash of the
//!    IR, and carrying content commitments ([`anchor`]);
//! 3. **commit**: inside the harness's compaction (Claude Code's `session.compact` plugin event),
//!    [`commit`] compares the record against the conversation the harness is about to compact:
//!    the checkpoint still admits the transcript, every commitment is there in order, no tool
//!    call is split from its result, the result fits, the record is not too old;
//! 4. **miss**: anything else is a [`Miss`], and the harness compacts as it would have (its own
//!    compaction, native precompute included), within the caller's deadline.
//!
//! This module is the policy-free core: the record, the commitments and their check, which
//! instruction a summary was written to ([`Profile`]), how a window is tracked as messages stream
//! past ([`window::Window`]), the prompts ([`prompt`]) and the record store ([`store`]). The
//! resident driver and the model calls are the CLI's (`cv watch --kind precompact`).

use serde::{Deserialize, Serialize};

pub mod anchor;
pub mod carry;
pub mod prompt;
pub mod request;
pub mod store;
pub mod window;

pub use anchor::{Anchor, MIN_ANCHOR_BYTES};
pub use request::{Instruction, RequestView, SessionId};
pub use window::Window;

/// The on-disk record version. A record of another version is not read (a miss, never a guess).
/// 2: the record carries its [`Key`].
pub const RECORD_VERSION: u32 = 2;

/// What a ready record is keyed by: the cv checkpoint of the transcript read that produced the
/// summarized prefix, and the harness's own id of the last summarized message. The checkpoint
/// says the transcript is the same file, only appended to, since the summary was cut (sampled,
/// so it accelerates the check and never authorizes the swap alone: the commitments do); the
/// anchor is the id the harness itself validates by (Claude Code's `precomputedAtUuid`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// The transcript the record summarizes.
    pub source: std::path::PathBuf,
    /// The transcript as it stood when the cut was taken.
    pub file: crate::source::FileId,
    /// The sampled bytes `0..file.len` of it.
    pub prefix: crate::source::Prefix,
    /// The harness's own id of the cut message.
    pub anchor: String,
}

impl Key {
    /// The key of a cut taken now: `source` as it stands, `anchor` the cut message's own id.
    /// `None` when the file cannot be read.
    pub fn take(source: &std::path::Path, anchor: String) -> Option<Key> {
        let file = std::fs::File::open(source).ok()?;
        let id = crate::source::FileId::of(&file.metadata().ok()?)?;
        let prefix = crate::source::Prefix::of(&file, id.len)?;
        Some(Key {
            source: source.to_path_buf(),
            file: id,
            prefix,
            anchor,
        })
    }

    /// The transcript is still this file, appended to or unchanged, its sampled prefix intact.
    pub fn admits_now(&self) -> bool {
        crate::source::FileId::at(&self.source).is_some_and(|now| self.file.admits(&self.prefix, &now, &self.source))
    }
}

/// Where a summary was cut: the last message it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cut {
    /// 0-based index of the cut message in the session's message stream.
    pub index: usize,
    /// The cut message's native id, when the harness gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The window's token fill at the cut, from the transcript's own usage records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Nothing but the last anchor's own message follows the last anchor up to the cut (no tool
    /// call, no tool result): a request whose last anchor is followed only by the compaction
    /// instruction has an empty tail.
    #[serde(default)]
    pub clean: bool,
}

/// The window a record summarizes: the span after the session's `boundaries`-th compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRef {
    /// Compactions the session had before this window began.
    pub boundaries: usize,
    /// 0-based index of the window's first message.
    pub start: usize,
}

/// One precomputed summary: harness-neutral, keyed by session, valid only while its anchors hold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    /// The harness name (`claude`, `codex`, ...).
    pub harness: String,
    /// The session id the harness sends with its requests.
    pub session: String,
    /// The checkpoint and harness anchor the record is keyed by; a record without one is never
    /// committed (a replay or a dry run builds such records).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Key>,
    pub window: WindowRef,
    pub cut: Cut,
    /// The content commitments: the window's prose and tool call ids up to the cut (see
    /// [`anchor`]), checked against the conversation the harness is about to compact.
    pub anchors: Vec<Anchor>,
    /// The [`Profile`] id: the harness instruction the summary was written to.
    pub profile: String,
    /// The model that wrote it.
    pub model: String,
    pub created: chrono::DateTime<chrono::Utc>,
    /// The cut this record extended, when it was written incrementally from an earlier record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Cut>,
    /// The harness's auto-compact point when the summary was written: the commit's fit budget
    /// is a fraction of it ([`Settings::fit_fraction`]). Unknown: no fit check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_at: Option<u64>,
    /// What the summary's input left out, stated (for example a per-tool-result byte budget).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bounds: Vec<String>,
    /// The narrative's `[m:ID]` cites that named no message of the window and were removed
    /// ([`carry::cites`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unresolved_cites: usize,
    /// The narrative part of the summary as the model wrote it: what the next extension reads
    /// as the summary so far. The carried sections are never fed back to be summarized again.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub narrative: String,
    /// The sections carried verbatim from generation to generation ([`carry`]).
    #[serde(default)]
    pub carry: carry::Carry,
    /// The summary as served: the narrative, the carried sections, then the person's words.
    pub text: String,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A harness's own compaction instruction, captured verbatim from a real request. cv never
/// writes a vendor's prompt: it records the one the harness sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// [`profile_id`] of `text`.
    pub id: String,
    pub harness: String,
    /// The client that sent it (a `User-Agent`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    pub captured: chrono::DateTime<chrono::Utc>,
    pub text: String,
}

/// A profile's id: the first 16 hex digits of the instruction's SHA-256.
pub fn profile_id(instruction: &str) -> String {
    let mut id = crate::digest::sha256_hex(instruction.as_bytes());
    id.truncate(16);
    id
}

/// Why a record does not answer a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    /// The record holds no anchor, so nothing ties it to the request.
    NoAnchors,
    /// The request's instruction is not the one the summary was written to.
    Profile { record: String, request: String },
    /// Anchor `index` of `of` was not found after the previous one.
    Anchor { index: usize, of: usize },
    /// The record has no [`Key`] (a replay's or a dry run's), so it is never committed.
    NoKey,
    /// The transcript is not the file the summary was cut from, appended to: it was replaced,
    /// shrunk or rewritten under the sampled bytes.
    Checkpoint,
    /// The compaction carries instructions of its own (`/compact <focus>`), which the summary
    /// was not written to.
    Focus,
    /// The record is older than [`MAX_AGE_SECS`].
    TooOld { secs: i64 },
    /// The summary and the messages kept after it would not fit the budget.
    WouldNotFit { tokens: u64, budget: u64 },
}

impl Miss {
    /// The miss's class: one word per variant, the key `report` counts misses by.
    pub fn class(&self) -> &'static str {
        match self {
            Miss::NoAnchors => "no_anchors",
            Miss::Profile { .. } => "profile",
            Miss::Anchor { .. } => "covered_messages_changed",
            Miss::NoKey => "no_key",
            Miss::Checkpoint => "checkpoint_refused",
            Miss::Focus => "focus_instructions",
            Miss::TooOld { .. } => "too_old",
            Miss::WouldNotFit { .. } => "would_not_fit",
        }
    }
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Miss::NoAnchors => write!(f, "the record has no anchors"),
            Miss::Profile { record, request } => write!(
                f,
                "the request's instruction (profile {request}) is not the one the summary was written to ({record})"
            ),
            Miss::Anchor { index, of } => write!(
                f,
                "anchor {} of {of} is not in the request after the previous one (history changed under the summary)",
                index + 1
            ),
            Miss::NoKey => write!(f, "the record has no checkpoint and anchor id to commit by"),
            Miss::Checkpoint => write!(
                f,
                "the transcript is no longer the file the summary was cut from, appended to"
            ),
            Miss::Focus => write!(
                f,
                "the compaction has instructions of its own; the summary was not written to them"
            ),
            Miss::TooOld { secs } => write!(f, "the summary is {secs} s old (the limit is {MAX_AGE_SECS} s)"),
            Miss::WouldNotFit { tokens, budget } => write!(
                f,
                "the summary and the kept messages come to about {tokens} tokens, over the {budget}-token budget"
            ),
        }
    }
}

/// A record that answers a request: the request messages from `tail_from` on (the instruction
/// excluded) are what the summary does not cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub tail_from: usize,
    /// Anchors matched (all of them).
    pub anchors: usize,
}

/// Check `record` against the request the harness sent. Every anchor must appear in order in
/// the request's prose; the first one missing is the miss. The tail starts at the message
/// holding the last anchor, or right after it when the cut was clean (see [`Cut::clean`]), so a
/// tail can repeat a little of the summary but never leaves a gap.
pub fn validate(request: &RequestView, record: &Record) -> Result<Match, Miss> {
    if record.anchors.is_empty() {
        return Err(Miss::NoAnchors);
    }
    if let Some(instruction) = &request.instruction {
        let id = profile_id(&instruction.text);
        if id != record.profile {
            return Err(Miss::Profile {
                record: record.profile.clone(),
                request: id,
            });
        }
    }
    let stream = request.prose();
    let ends = anchor::find_in_order(&stream.text, &record.anchors).map_err(|index| Miss::Anchor {
        index,
        of: record.anchors.len(),
    })?;
    let last = *ends.last().expect("anchors are not empty");
    let message = stream.message_at(last.saturating_sub(1));
    let tail_from = if record.cut.clean { message + 1 } else { message };
    Ok(Match {
        tail_from,
        anchors: ends.len(),
    })
}

/// A record older than this is never committed: Claude Code's own limit for its precomputed
/// summary (`too_old`, 604,800,000 ms). A safety cap.
pub const MAX_AGE_SECS: i64 = 7 * 24 * 3600;

/// Bytes per token in the commit's fit estimate: the conversation is counted in bytes (the
/// harness hands no token counts per message), so the estimate is coarse, and only decides
/// whether to fall back to the harness's own compaction.
pub const BYTES_PER_TOKEN: u64 = 4;

/// A committed record: the summary replaces the conversation's messages before `keep_from`, and
/// the messages from `keep_from` on are kept as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub keep_from: usize,
    /// Commitments matched (all of them).
    pub anchors: usize,
    /// The estimated tokens of the summary plus the kept messages.
    pub tokens: u64,
}

/// The commit check, run inside the harness's compaction on the conversation it is about to
/// compact (`view`, the `session.compact` input). In order: the record has a key and its
/// checkpoint still admits the transcript; it is not too old; the compaction carries no
/// instructions of its own; every commitment is in the conversation in order ([`validate`]);
/// the kept messages start on a whole tool call (a result whose call the summary covers moves
/// the cut back to that call); and the summary plus the kept messages fit `budget` tokens.
pub fn commit(view: &RequestView, record: &Record, budget: Option<u64>) -> Result<Commit, Miss> {
    let key = record.key.as_ref().ok_or(Miss::NoKey)?;
    if !key.admits_now() {
        return Err(Miss::Checkpoint);
    }
    let secs = (chrono::Utc::now() - record.created).num_seconds();
    if secs > MAX_AGE_SECS {
        return Err(Miss::TooOld { secs });
    }
    if view.focus.as_deref().is_some_and(|f| !f.trim().is_empty()) {
        return Err(Miss::Focus);
    }
    let matched = validate(view, record)?;
    let keep_from = whole_groups(view, matched.tail_from);
    let kept: u64 = view.messages[keep_from.min(view.messages.len())..]
        .iter()
        .map(|m| m.bytes as u64)
        .sum();
    let tokens = (kept + record.text.len() as u64) / BYTES_PER_TOKEN;
    if let Some(budget) = budget.filter(|b| tokens > *b) {
        return Err(Miss::WouldNotFit { tokens, budget });
    }
    Ok(Commit {
        keep_from,
        anchors: matched.anchors,
        tokens,
    })
}

/// The first kept message moved back until no kept tool result answers a call the summary
/// covers (Claude Code refuses such a reuse as `unseen_reply`; an API refuses a result without
/// its call).
fn whole_groups(view: &RequestView, from: usize) -> usize {
    let mut from = from.min(view.messages.len());
    while from > 0 {
        let kept = &view.messages[from..];
        let calls: std::collections::HashSet<&str> = kept
            .iter()
            .flat_map(|m| m.calls.iter().map(|(_, id)| id.as_str()))
            .collect();
        if !kept
            .iter()
            .any(|m| m.results.iter().any(|id| !calls.contains(id.as_str())))
        {
            break;
        }
        from -= 1;
    }
    from
}

/// Which compaction serves a session whose harness hands cv its compaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Serve {
    /// The harness's own (native precompute included); cv records every compaction.
    #[default]
    Native,
    /// cv's record when its commit check passes; the harness's own on every miss.
    Cv,
}

impl std::str::FromStr for Serve {
    type Err = String;
    fn from_str(s: &str) -> Result<Serve, String> {
        match s {
            "native" => Ok(Serve::Native),
            "cv" => Ok(Serve::Cv),
            other => Err(format!("`{other}` is not a compaction to serve (native, cv)")),
        }
    }
}

impl Serve {
    pub fn as_str(self) -> &'static str {
        match self {
            Serve::Native => "native",
            Serve::Cv => "cv",
        }
    }
}

/// The threshold settings the driver reads, with their named defaults. Each is a cost-only
/// setting: it decides when work runs, never what a served summary says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Start precomputing at this fraction of the token count where the harness auto-compacts.
    pub start_fraction: f64,
    /// The harness's auto-compact point in tokens. Unset: learned from the session's own
    /// auto-compaction records (`preTokens`), else the lowest seen for its harness and model.
    pub compact_at: Option<u64>,
    /// Extend the summary each time this many tokens have accumulated since the last cut.
    pub extend_tokens: u64,
    /// When a session has been idle this many seconds with any new message since the last cut,
    /// extend now, so the cut sits at the end of the conversation when compaction comes.
    pub idle_secs: u64,
    /// Bytes of each tool result the summary model receives (head and tail kept, the cut named).
    pub tool_result_bytes: usize,
    /// The summary model (a provider model id). Unset: the provider's default.
    pub model: Option<String>,
    /// Summary jobs running at once.
    pub jobs: usize,
    /// Follow sessions touched within this many seconds of startup (and every new one after).
    pub live_within_secs: u64,
    /// Which compaction serves (the host's default; a seat's own setting wins). Chosen by
    /// measurement (`cv watch --kind precompact report`), never assumed.
    pub serve: Serve,
    /// Also prepare summaries for sessions whose harness precomputes its own: a seat that serves
    /// cv's, and the comparison of the two, need them; otherwise cv stays idle there.
    pub prepare_native: bool,
    /// A captured [`Profile`] id to write summaries to. Unset: cv's own instruction
    /// ([`prompt::CV_INSTRUCTION`]).
    pub profile: Option<String>,
    /// Bytes of the window's prompts carried verbatim after the summary, newest first.
    pub owner_words_bytes: usize,
    /// Identifiers carried in the live-identifier section, the most recently seen kept.
    pub carry_ids: usize,
    /// Pointers to standing rules carried, the most recently used kept.
    pub carry_pointers: usize,
    /// A carried item no span has said or used again for this many windows retires (a ruling of
    /// the person's retires only when a later ruling of theirs supersedes it).
    pub retire_after: usize,
    /// Bytes of lines carried from the harness's own earlier summary, the most recently seen kept.
    pub standing_bytes: usize,
    /// Patterns (regular expressions) that find a standing rule's id in the span, the first
    /// capture group the id (else the whole match): what the summary's pointers to standing rules
    /// are chosen by ([`carry::Pointer`]). Empty: no pointers. A fleet's rule store names its own.
    pub rule_patterns: Vec<String>,
    /// The summary and the messages kept after it must fit in this fraction of the auto-compact
    /// point, or the commit misses.
    pub fit_fraction: f64,
}

/// The opening of every summary cv serves, shared with what reads it back: the harness stores a
/// served summary as a plain message of the person's role, so this is how the next window tells
/// it from something the person said.
pub const SERVED_PREAMBLE: &str = "This session continues from an earlier conversation that cv compacted";

/// [`Settings::start_fraction`]: 70% of the auto-compact point.
pub const START_FRACTION: f64 = 0.70;
/// [`Settings::extend_tokens`].
pub const EXTEND_TOKENS: u64 = 20_000;
/// [`Settings::idle_secs`].
pub const IDLE_SECS: u64 = 45;
/// [`Settings::tool_result_bytes`].
pub const TOOL_RESULT_BYTES: usize = 2048;
/// [`Settings::jobs`].
pub const JOBS: usize = 2;
/// [`Settings::live_within_secs`]: two hours.
pub const LIVE_WITHIN_SECS: u64 = 7200;
/// [`Settings::owner_words_bytes`].
pub const OWNER_WORDS_BYTES: usize = 16 * 1024;
/// [`Settings::carry_ids`]: each id carries the message it was last seen in, about 50 tokens a
/// line, so 200 cost about 10k tokens of a summary; 25 cost about 1.5k.
pub const CARRY_IDS: usize = 25;
/// [`Settings::carry_pointers`].
pub const CARRY_POINTERS: usize = 20;
/// [`Settings::retire_after`].
pub const RETIRE_AFTER: usize = 3;
/// [`Settings::standing_bytes`]: about 3k tokens of a summary, the size of what the 2026-10-31
/// probes on real windows found the narrative and the other sections had lost.
pub const STANDING_BYTES: usize = 12 * 1024;
/// [`Settings::fit_fraction`]: half the auto-compact point, so the compacted window has room to
/// work before it fills again.
pub const FIT_FRACTION: f64 = 0.5;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            start_fraction: START_FRACTION,
            compact_at: None,
            extend_tokens: EXTEND_TOKENS,
            idle_secs: IDLE_SECS,
            tool_result_bytes: TOOL_RESULT_BYTES,
            model: None,
            jobs: JOBS,
            live_within_secs: LIVE_WITHIN_SECS,
            serve: Serve::Native,
            prepare_native: false,
            profile: None,
            owner_words_bytes: OWNER_WORDS_BYTES,
            carry_ids: CARRY_IDS,
            carry_pointers: CARRY_POINTERS,
            retire_after: RETIRE_AFTER,
            standing_bytes: STANDING_BYTES,
            rule_patterns: Vec::new(),
            fit_fraction: FIT_FRACTION,
        }
    }
}

/// What the driver should do for a session now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// Nothing: the reason says why.
    Wait(&'static str),
    /// Write the window's first summary, up to the newest message.
    Start,
    /// Extend the record from its cut to the newest message.
    Extend,
}

/// Decide whether a session's summary is due. Pure: `idle` is seconds since the session's last
/// message, `compact_at` the auto-compact point in force, `record` the cut of the current
/// window's record if one exists.
pub fn due(settings: &Settings, window: &Window, compact_at: Option<u64>, record: Option<&Cut>, idle: u64) -> Due {
    if window.native_precompute() && !settings.prepare_native {
        return Due::Wait("the harness precomputes its own compactions");
    }
    let Some(fill) = window.fill() else {
        return Due::Wait("no usage record yet");
    };
    let Some(compact_at) = compact_at else {
        return Due::Wait("auto-compact point unknown (set precompact.compact_at)");
    };
    let start = (compact_at as f64 * settings.start_fraction) as u64;
    if fill < start {
        return Due::Wait("below the start threshold");
    }
    let Some(newest) = window.newest_index() else {
        return Due::Wait("empty window");
    };
    let Some(cut) = record else {
        return Due::Start;
    };
    if newest <= cut.index {
        return Due::Wait("the summary is current");
    }
    let grown = fill.saturating_sub(cut.tokens.unwrap_or(0));
    if grown >= settings.extend_tokens || idle >= settings.idle_secs {
        Due::Extend
    } else {
        Due::Wait("waiting for more tokens or for the session to go idle")
    }
}

#[cfg(test)]
mod tests;
