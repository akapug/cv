//! Precompaction: a session's compaction summary, written before the harness asks for it.
//!
//! A harness compacts when its context window fills: it sends the whole window to a model with
//! its own summary instruction and waits for the answer (Claude Code through a proxy to a Codex
//! model: a median of about 330 s). Most of that window is old and stable, so the summary can be
//! written early, off the critical path, and kept current as the session grows. When the harness
//! finally asks, the answer is ready, or only the tail since the last cut is left to summarize.
//!
//! This module is the policy-free core: what a precomputed summary is ([`Record`]), how a later
//! compaction request is recognized as the same conversation ([`anchor`], [`validate`]), which
//! instruction the summary was written to ([`Profile`]), how a window is tracked as messages
//! stream past ([`window::Window`]), and the prompts for writing and extending a summary
//! ([`prompt`]). No I/O policy lives here beyond the record store ([`store`]); the resident driver
//! and the model calls are the CLI's (`cv precompact`).
//!
//! Safety rule: a summary is served only when every anchor it was cut at appears, in order, in the
//! request the harness actually sent (see [`validate`]). A stale summary is never served; a miss
//! falls through to the harness's own compaction.

use serde::{Deserialize, Serialize};

pub mod anchor;
pub mod prompt;
pub mod request;
pub mod store;
pub mod window;

pub use anchor::{Anchor, MIN_ANCHOR_BYTES};
pub use request::{Instruction, RequestView, SessionId};
pub use window::Window;

/// The on-disk record version. A record of another version is not read (a miss, never a guess).
pub const RECORD_VERSION: u32 = 1;

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
    pub window: WindowRef,
    pub cut: Cut,
    /// The fingerprint of the window up to the cut (see [`anchor`]).
    pub anchors: Vec<Anchor>,
    /// The [`Profile`] id: the harness instruction the summary was written to.
    pub profile: String,
    /// The model that wrote it.
    pub model: String,
    pub created: chrono::DateTime<chrono::Utc>,
    /// The cut this record extended, when it was written incrementally from an earlier record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Cut>,
    /// What the summary's input left out, stated (for example a per-tool-result byte budget).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bounds: Vec<String>,
    /// The summary, as the harness's own compaction would have returned it.
    pub text: String,
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
}

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
    if window.native_precompute() {
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
