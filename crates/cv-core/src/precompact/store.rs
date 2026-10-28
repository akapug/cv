//! Where precomputed summaries live: `$CLUSTERVISION_HOME/precompact/` (default
//! `~/.clustervision/precompact/`).
//!
//! - `records/<harness>/<session>.json`: the session's current [`Record`], replaced whole on each
//!   extension (atomic publish, so a reader sees the old record or the new one);
//! - `profiles/<id>.json`: each harness instruction ever captured ([`Profile`]);
//! - `events.jsonl`: one line per summary written, request answered or missed, and compaction
//!   observed: the measurements `cv precompact report` reads.

use super::{Profile, Record, RECORD_VERSION};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The store's root.
pub fn root() -> Result<PathBuf> {
    Ok(crate::env::Env::current()
        .cv_home()
        .context("no home directory for the precompact store")?
        .join("precompact"))
}

/// A path component from an id: anything outside `[A-Za-z0-9._-]` becomes `_`, and a leading dot
/// is not kept, so an id never names another directory.
fn component(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.trim_start_matches('.').to_owned()
}

pub fn record_path(root: &Path, harness: &str, session: &str) -> PathBuf {
    root.join("records")
        .join(component(harness))
        .join(format!("{}.json", component(session)))
}

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(!crate::env::frozen(), "--frozen forbids writing the precompact store");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    crate::publish::publish_reader(path, "precompact", bytes, crate::publish::Claim::Replace)
        .with_context(|| format!("writing {}", path.display()))
}

/// Save a session's record, replacing its previous one.
pub fn save_record(root: &Path, record: &Record) -> Result<PathBuf> {
    let path = record_path(root, &record.harness, &record.session);
    publish(&path, &serde_json::to_vec(record)?)?;
    Ok(path)
}

/// The session's record, `None` when there is none or it is of another version.
pub fn load_record(root: &Path, harness: &str, session: &str) -> Result<Option<Record>> {
    let path = record_path(root, harness, session);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let record: Record = serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?;
    Ok((record.v == RECORD_VERSION).then_some(record))
}

/// Every record of `harness` (all harnesses when `None`): `(path, record)`.
pub fn records(root: &Path, harness: Option<&str>) -> Result<Vec<(PathBuf, Record)>> {
    let base = root.join("records");
    let dirs: Vec<PathBuf> = match harness {
        Some(h) => vec![base.join(component(h))],
        None => match std::fs::read_dir(&base) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        },
    };
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(record) = serde_json::from_slice::<Record>(&std::fs::read(&path)?) {
                if record.v == RECORD_VERSION {
                    out.push((path, record));
                }
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.1.created));
    Ok(out)
}

/// Remove a session's record (its window ended).
pub fn remove_record(root: &Path, harness: &str, session: &str) -> Result<bool> {
    anyhow::ensure!(!crate::env::frozen(), "--frozen forbids writing the precompact store");
    match std::fs::remove_file(record_path(root, harness, session)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Store a captured profile; `false` when that instruction was already stored.
pub fn save_profile(root: &Path, profile: &Profile) -> Result<bool> {
    let path = root.join("profiles").join(format!("{}.json", component(&profile.id)));
    if path.exists() {
        return Ok(false);
    }
    publish(&path, &serde_json::to_vec_pretty(profile)?)?;
    Ok(true)
}

/// Every stored profile, newest capture first.
pub fn profiles(root: &Path) -> Result<Vec<Profile>> {
    let dir = root.join("profiles");
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if let Ok(p) = serde_json::from_slice::<Profile>(&std::fs::read(e.path())?) {
            out.push(p);
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.captured));
    Ok(out)
}

/// The newest profile captured for `harness`.
pub fn latest_profile(root: &Path, harness: &str) -> Result<Option<Profile>> {
    Ok(profiles(root)?.into_iter().find(|p| p.harness == harness))
}

/// Append one measurement line to `events.jsonl`.
pub fn event(root: &Path, mut value: serde_json::Value) -> Result<()> {
    if crate::env::frozen() {
        return Ok(());
    }
    use std::io::Write as _;
    if let Some(map) = value.as_object_mut() {
        map.entry("at")
            .or_insert_with(|| chrono::Utc::now().to_rfc3339().into());
    }
    std::fs::create_dir_all(root)?;
    let mut line = serde_json::to_vec(&value)?;
    line.push(b'\n');
    // One write of one line with O_APPEND: concurrent writers' lines do not interleave.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("events.jsonl"))?
        .write_all(&line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_never_name_another_directory() {
        assert_eq!(component("../../etc"), "_.._etc");
        assert_eq!(component("a/b"), "a_b");
        assert_eq!(component("7e923d7a-382e"), "7e923d7a-382e");
    }
}
