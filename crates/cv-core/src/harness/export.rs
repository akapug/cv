//! **Account data-export** adapters — the archives you download from ChatGPT ("Export data") and
//! Claude.ai. Both pack *many* conversations into one big JSON file (`conversations.json`, often split
//! `conversations-000.json … -NNN.json`), so unlike the per-session harnesses one file yields many
//! [`SessionRef`]s. Two shapes, sniffed by content (never a filename):
//!
//! - **ChatGPT export** (`Harness::ChatGptExport`): each conversation has a `mapping` — a DAG of
//!   `node_id -> {parent, children[], message}` — with `current_node` as the active leaf. We walk the
//!   root→`current_node` chain (the live branch). A message has `author.role`, `content.{content_type,
//!   parts|text}`, and `recipient` (≠`all` ⇒ a tool call). `default_model_slug` is the model.
//! - **Claude.ai export** (`Harness::ClaudeExport`): each conversation has `chat_messages[]`, linear
//!   via `parent_message_uuid`; `sender` ∈ human/assistant; `content[]` blocks are
//!   text/thinking/tool_use/tool_result.
//!
//! Distinct from `claude` (Claude Code JSONL). Exports have no fixed home, so discovery is
//! **opt-in**: the sources registered with `cv config --add-export <path>`, unioned with
//! `$CV_EXPORTS` (`:`-separated dirs/files), scanned for `conversations*.json`. With nothing
//! registered no scan happens (see [`export_dirs`]).

use super::wire::anthropic;
use super::{ts_from_value, Adapter};
use crate::ir::*;
use crate::stream::{MessageSink, ParseOptions};
use anyhow::{Context, Result};

mod native;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ChatGpt,
    Claude,
}

impl Kind {
    fn harness(self) -> Harness {
        match self {
            Kind::ChatGpt => Harness::ChatGptExport,
            Kind::Claude => Harness::ClaudeExport,
        }
    }
}

// ── Adapters (thin wrappers over the shared engine, one per export shape) ──────

pub struct ChatGptExport;
pub struct ClaudeExport;
impl Adapter for ChatGptExport {
    fn harness(&self) -> Harness {
        Harness::ChatGptExport
    }
    fn roots(&self) -> Vec<PathBuf> {
        export_dirs()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind(Kind::ChatGpt))
    }
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(Kind::ChatGpt, r, opts, sink))
    }
}

impl Adapter for ClaudeExport {
    fn harness(&self) -> Harness {
        Harness::ClaudeExport
    }
    fn roots(&self) -> Vec<PathBuf> {
        export_dirs()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind(Kind::Claude))
    }
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(Kind::Claude, r, opts, sink))
    }
}

impl ChatGptExport {
    /// Discover only supplied export files/directories, without consulting configuration.
    pub fn with_roots(roots: Vec<PathBuf>) -> impl Adapter {
        RootedExport {
            kind: Kind::ChatGpt,
            roots,
        }
    }
}
impl ClaudeExport {
    /// Discover only supplied export files/directories, without consulting configuration.
    pub fn with_roots(roots: Vec<PathBuf>) -> impl Adapter {
        RootedExport {
            kind: Kind::Claude,
            roots,
        }
    }
}
struct RootedExport {
    kind: Kind,
    roots: Vec<PathBuf>,
}
impl Adapter for RootedExport {
    fn harness(&self) -> Harness {
        self.kind.harness()
    }
    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind_in(
            self.kind,
            files_under(self.roots.clone(), is_conversations_file),
        ))
    }
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(self.kind, r, opts, sink))
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Where to look for export archives — the registered sources from the config file
/// (`cv config --add-export <path>`; see [`crate::config`]), unioned with `$CV_EXPORTS` (a
/// `:`-separated ad-hoc list). **Opt-in**: with nothing registered and the env unset, export discovery
/// is a no-op, so the default `cv ls`/`discover_all` never pays to scan (account exports are huge —
/// enumerating thousands of conversations across multi-MB files takes seconds).
pub(crate) fn export_dirs() -> Vec<PathBuf> {
    let mut dirs = crate::config::load().exports;
    if let Ok(spec) = crate::env::var("CV_EXPORTS") {
        dirs.extend(spec.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    dirs.retain(|p| p.exists());
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Every `conversations*.json` under the export dirs.
fn export_files() -> Vec<PathBuf> {
    files_under(export_dirs(), is_conversations_file)
}

/// Every file under `roots` matching `pred` (depth ≤ 2; a dir entry is scanned, a file entry is
/// taken directly), sorted and deduped. Shared with [`opensession`](super::opensession), which
/// discovers `.opensession.json` documents through the same registered sources — they have no
/// fixed home either.
pub(crate) fn files_under(roots: Vec<PathBuf>, pred: fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        if root.is_file() {
            if pred(&root) {
                out.push(root);
            }
            continue;
        }
        // root dir + one level of subdirs (an extracted export is usually a subfolder)
        for depth1 in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let p = depth1.path();
            if p.is_file() && pred(&p) {
                out.push(p);
            } else if p.is_dir() {
                for depth2 in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    let q = depth2.path();
                    if q.is_file() && pred(&q) {
                        out.push(q);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_conversations_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "conversations.json" || (n.starts_with("conversations") && n.ends_with(".json")))
}

/// Enumerate every conversation of `kind` across all export files as lightweight [`SessionRef`]s.
/// Reads each file once: parse the array, sniff the kind from its first element, then enumerate.
/// Dedups by conversation id (keeping the first) — overlapping exports (e.g. a loose
/// `conversations-000.json` plus an extracted full archive) commonly repeat the same conversation.
fn discover_kind(kind: Kind) -> Vec<SessionRef> {
    discover_kind_in(kind, export_files())
}

fn discover_kind_in(kind: Kind, paths: Vec<PathBuf>) -> Vec<SessionRef> {
    native::discover(kind, paths)
}

// ── Parse one conversation ──────────────────────────────────────────────────

fn role_from_str(s: &str) -> Role {
    match s {
        "user" | "human" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::System,
    }
}

#[cfg(test)]
mod tests;
