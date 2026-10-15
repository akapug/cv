//! Store helpers shared by every adapter: the on-disk plumbing that is the same whichever harness
//! wrote the files. An adapter imports these and keeps only what is genuinely its own — where its
//! files are and what its records mean.

#[cfg(test)]
use anyhow::Result;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

pub(crate) mod bytes;
pub(crate) mod document;
pub(crate) mod json;
pub(crate) mod jsonl;
mod jsonl_write;
pub(crate) use jsonl_write::{publish_stage, JsonlStage};
pub(crate) mod text;

#[cfg(feature = "sqlite")]
pub mod sqlite;
pub(crate) mod zst;

/// `$XDG_DATA_HOME` when set to an absolute path (the spec says a relative value is ignored), else
/// `~/.local/share`. The base of every XDG-shaped store (opencode, devin, goose, zed); `None` only
/// when there is no home directory at all.
pub(crate) fn xdg_data_dir() -> Option<PathBuf> {
    crate::env::Env::current().data_home()
}

/// Write `lines` as JSONL (one compact JSON value per line) to `path`, replacing the file.
#[cfg(test)]
pub(crate) fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut stage = JsonlStage::new();
    for value in lines {
        stage.push(value)?;
    }
    stage.publish(path, crate::publish::Claim::Replace)
}
