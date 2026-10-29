//! Where precomputed summaries live: `$CLUSTERVISION_HOME/precompact/` (default
//! `~/.clustervision/precompact/`).
//!
//! - `records/<harness>/<session>.json`: the session's current [`Record`], replaced whole on each
//!   extension (atomic publish, so a reader sees the old record or the new one);
//! - `carry/<harness>/<session>.json`: the sections a window's last record carried, which the
//!   next window's first summary starts from ([`super::carry`]);
//! - `leases/<harness>/<session>.json`: the owner of the summary being prepared ([`take_lease`]);
//! - `profiles/<id>.json`: each harness instruction ever captured ([`Profile`]);
//! - `events.jsonl`: one line per summary written, commit served or missed, and compaction
//!   observed (cv's and the harness's own): the measurements `cv watch --kind precompact report`
//!   reads.

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

fn carry_path(root: &Path, harness: &str, session: &str) -> PathBuf {
    root.join("carry")
        .join(component(harness))
        .join(format!("{}.json", component(session)))
}

/// Keep a session's carried sections past the window they were written in (its compaction).
pub fn save_carry(root: &Path, harness: &str, session: &str, carry: &super::carry::Carry) -> Result<()> {
    publish(&carry_path(root, harness, session), &serde_json::to_vec(carry)?)
}

/// The carried sections the session's last window ended with, if any.
pub fn load_carry(root: &Path, harness: &str, session: &str) -> Option<super::carry::Carry> {
    serde_json::from_slice(&std::fs::read(carry_path(root, harness, session)).ok()?).ok()
}

/// A prepare step's lease: who is writing the session's next summary, for which generation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Lease {
    pub pid: u32,
    /// `<compactions before the window>:<cut index>:<cut message id>`.
    pub generation: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

pub fn lease_path(root: &Path, harness: &str, session: &str) -> PathBuf {
    root.join("leases")
        .join(component(harness))
        .join(format!("{}.json", component(session)))
}

/// Take the lease on a session's next summary (the prepare step): one owner per context
/// generation, across every follower on the host. `Ok(false)` when another owner holds a lease
/// younger than `stale_after` (a summary call's own timeout); an older one, or this process's
/// own, is taken over.
pub fn take_lease(
    root: &Path,
    harness: &str,
    session: &str,
    generation: &str,
    stale_after: std::time::Duration,
) -> Result<bool> {
    use std::io::Write as _;
    anyhow::ensure!(!crate::env::frozen(), "--frozen forbids writing the precompact store");
    let path = lease_path(root, harness, session);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let lease = Lease {
        pid: std::process::id(),
        generation: generation.to_owned(),
        at: chrono::Utc::now(),
    };
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(&serde_json::to_vec(&lease)?)?;
                return Ok(true);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let held = std::fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Lease>(&b).ok());
                let live = held.is_some_and(|h| {
                    h.pid != lease.pid && (lease.at - h.at).to_std().is_ok_and(|age| age < stale_after)
                });
                if live {
                    return Ok(false);
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
                }
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    Ok(false)
}

/// Give a session's lease back (its summary is written, or failed).
pub fn release_lease(root: &Path, harness: &str, session: &str) {
    let _ = std::fs::remove_file(lease_path(root, harness, session));
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
    fn a_lease_has_one_owner_until_it_is_released_or_stale() {
        let root = std::env::temp_dir().join(format!("cv-precompact-lease-{}", uuid::Uuid::new_v4()));
        let hour = std::time::Duration::from_secs(3600);
        assert!(take_lease(&root, "claude", "s", "0:4:u", hour).unwrap());
        // Another owner's lease, young: refused; this process's own: taken over.
        let other = Lease {
            pid: std::process::id() + 1,
            generation: "0:4:u".into(),
            at: chrono::Utc::now(),
        };
        std::fs::write(lease_path(&root, "claude", "s"), serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(!take_lease(&root, "claude", "s", "0:5:v", hour).unwrap());
        // Stale: taken over.
        assert!(take_lease(&root, "claude", "s", "0:5:v", std::time::Duration::ZERO).unwrap());
        release_lease(&root, "claude", "s");
        assert!(!lease_path(&root, "claude", "s").exists());
        assert!(take_lease(&root, "claude", "s", "0:6:w", hour).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ids_never_name_another_directory() {
        assert_eq!(component("../../etc"), "_.._etc");
        assert_eq!(component("a/b"), "a_b");
        assert_eq!(component("7e923d7a-382e"), "7e923d7a-382e");
    }
}
