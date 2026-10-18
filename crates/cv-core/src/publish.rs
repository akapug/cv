//! Staged publication, the one way cv puts a finished file under a name readers may already watch.
//!
//! A [`Staged`] file is written under a private temporary name in the destination's directory,
//! fsynced, and then claimed by name in one step, after which the directory is fsynced. A reader
//! of the destination, including a live mmap, sees no file, the whole previous file, or the whole
//! new one; never a truncated or partial one.
//!
//! - [`Claim::New`] refuses any existing name, a dangling symlink included, with [`Exists`]. It
//!   hard-links the temporary to the destination. Where the filesystem has no hard links (FAT,
//!   exFAT, some network mounts) it renames without replacing instead: `renamex_np(RENAME_EXCL)`
//!   on macOS, `renameat2(RENAME_NOREPLACE)` on Linux. Without either exclusive operation it fails
//!   closed: checking a name before an ordinary rename could overwrite a concurrent writer.
//! - [`Claim::Replace`] renames over the destination. The name moves to the new inode; a reader
//!   holding the old one keeps the old bytes, and a symlink at the destination is replaced, not
//!   followed.
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// How [`Staged::publish`] claims its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    New,
    Replace,
}

/// A [`Claim::New`] destination was already taken. Callers that refuse an existing output before
/// writing return it too, so every refusal reads the same and the CLI exits 2 on it.
#[derive(Debug, thiserror::Error)]
#[error("{} already exists, refusing to overwrite it{hint}", path.display())]
pub struct Exists {
    pub path: PathBuf,
    /// What the caller can do instead, `" (pick another --to)"`, or empty.
    pub hint: &'static str,
}

/// Refuse `path` if anything, a dangling symlink included, holds the name.
pub fn refuse_existing(path: &Path, hint: &'static str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(Exists {
            path: path.to_owned(),
            hint,
        }
        .into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// An exclusively created temporary that only this operation owns; dropped unpublished, it is
/// removed. Writes go straight to the file (wrap it in a `BufWriter` for small writes).
pub struct Staged {
    path: PathBuf,
    file: Option<File>,
}

impl Staged {
    /// A new temporary `.cv-<tag>-<uuid>.tmp` in `dir`, created with `mode` (before the umask) on
    /// unix. Publication keeps that mode.
    pub fn create(dir: &Path, tag: &str, mode: u32) -> Result<Self> {
        let path = dir.join(format!(".cv-{tag}-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let file = options
            .open(&path)
            .with_context(|| format!("creating a temporary in {}", dir.display()))?;
        Ok(Self { path, file: Some(file) })
    }

    /// [`Self::create`] in `destination`'s directory, so publication never crosses a filesystem.
    pub fn beside(destination: &Path, tag: &str, mode: u32) -> Result<Self> {
        let dir = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self::create(dir, tag, mode)
    }

    /// The temporary's own name, valid until publication.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("a staged file is open until published")
    }

    /// Make the bytes durable, claim `destination`, then make the name durable. Returns the
    /// published file's handle (still open, positioned at its end).
    pub fn publish(self, destination: &Path, claim: Claim) -> Result<File> {
        self.publish_linking(destination, claim, |from, to| std::fs::hard_link(from, to))
    }

    fn publish_linking(
        mut self,
        destination: &Path,
        claim: Claim,
        link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<File> {
        let context = || format!("publishing {}", destination.display());
        let file = self.file.take().expect("a staged file is published once");
        file.sync_all().with_context(context)?;
        let claimed = match claim {
            Claim::Replace => std::fs::rename(&self.path, destination),
            Claim::New => match link(&self.path, destination) {
                Ok(()) => {
                    // The destination now names the inode; the temporary name is just a second link.
                    let _ = std::fs::remove_file(&self.path);
                    Ok(())
                }
                Err(e) if links_unsupported(&e) => rename_no_replace(&self.path, destination),
                Err(e) => Err(e),
            },
        };
        match claimed {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Exists {
                    path: destination.to_owned(),
                    hint: "",
                }
                .into())
            }
            Err(e) => return Err(e).with_context(context),
        }
        // The temporary name is gone (renamed or unlinked); Drop must not remove anything now.
        self.path = PathBuf::new();
        // The name is already claimed. A directory that cannot be synced loses durability of the
        // name, not correctness, so it does not turn a completed publication into a failure.
        sync_parent(destination);
        Ok(file)
    }
}

impl Write for Staged {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file().write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file().flush()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Close first: some platforms cannot unlink an open file.
        self.file = None;
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Undo a publication whose companion then failed, returning `cause` with any failure to undo it.
/// Only the file `published` refers to is removed, never one someone else has put there since: the
/// name is moved to a private one in one step, and what it held is removed only if it is that file;
/// another writer's file is put back without replacing a newer one. Without inode identity (off
/// Unix) nothing can be checked, so nothing is removed and the error says so.
pub fn retract(destination: &Path, published: &File, cause: anyhow::Error) -> anyhow::Error {
    match retract_racing(destination, published, || {}) {
        Ok(()) => cause,
        Err(e) => cause.context(format!("{} is left published: {e:#}", destination.display())),
    }
}

/// [`retract`], running `race` between looking at the name and taking it.
fn retract_racing(destination: &Path, published: &File, race: impl FnOnce()) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ours = published.metadata()?;
        let is_ours = |m: &std::fs::Metadata| m.dev() == ours.dev() && m.ino() == ours.ino();
        match std::fs::symlink_metadata(destination) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(named) if !is_ours(&named) => return Ok(()),
            Ok(_) => {}
        }
        race();
        // The name can change hands after that look; the rename takes exactly what it holds now.
        let aside = destination.with_file_name(format!(".cv-retract-{}.tmp", uuid::Uuid::new_v4()));
        match std::fs::rename(destination, &aside) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            taken => taken.context("retracting it")?,
        }
        let held = std::fs::symlink_metadata(&aside).with_context(|| format!("checking {}", aside.display()))?;
        if is_ours(&held) {
            return std::fs::remove_file(&aside).with_context(|| format!("removing {}", aside.display()));
        }
        rename_no_replace(&aside, destination)
            .with_context(|| format!("another writer's file is kept at {}", aside.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (destination, published, race);
        anyhow::bail!("this platform cannot check that the name still holds the published file")
    }
}

/// The filesystem cannot make a hard link here (as opposed to the name being taken or the caller
/// lacking permission to write the directory at all). Linux vfat answers EPERM.
fn links_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return [libc::EOPNOTSUPP, libc::ENOTSUP, libc::EPERM, libc::ENOSYS].contains(&code);
    }
    false
}

/// Rename `from` to `to` unless `to` exists, answering `AlreadyExists` if it does.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    let flag = libc::RENAME_EXCL;
    #[cfg(target_os = "linux")]
    let flag = libc::RENAME_NOREPLACE;
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    let exclusive = |from: &Path, to: &Path| rename_with(from, to, flag);
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    let exclusive = |_: &Path, _: &Path| None;
    rename_no_replace_using(from, to, exclusive)
}

fn rename_no_replace_using(
    from: &Path,
    to: &Path,
    exclusive: impl FnOnce(&Path, &Path) -> Option<io::Result<()>>,
) -> io::Result<()> {
    if let Some(result) = exclusive(from, to) {
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem does not support an atomic exclusive rename; destination left untouched",
    ))
}

/// `renamex_np` / `renameat2` with `flag`; `None` when this filesystem or kernel rejects the flag.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn rename_with(from: &Path, to: &Path, flag: libc::c_uint) -> Option<io::Result<()>> {
    use std::os::unix::ffi::OsStrExt;
    let (from_c, to_c) = match (
        std::ffi::CString::new(from.as_os_str().as_bytes()),
        std::ffi::CString::new(to.as_os_str().as_bytes()),
    ) {
        (Ok(from), Ok(to)) => (from, to),
        (Err(e), _) | (_, Err(e)) => return Some(Err(e.into())),
    };
    // SAFETY: both arguments are NUL-terminated paths that outlive the call.
    #[cfg(target_vendor = "apple")]
    let status = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), flag) } as i64;
    // SAFETY: as above; renameat2 takes two directory fds, two paths and a flag word.
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            flag,
        )
    } as i64;
    if status == 0 {
        return Some(Ok(()));
    }
    let error = io::Error::last_os_error();
    let unsupported = matches!(
        error.raw_os_error(),
        Some(code) if [libc::EINVAL, libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&code)
    );
    (!unsupported).then_some(Err(error))
}

/// Publish everything `bytes` yields at `destination` (see [`Staged`]). A new file is created
/// 0666 before the umask; a file it replaces keeps its permission bits. With [`Claim::Replace`]
/// the name moves to the new file: a symlink there is replaced rather than followed, and other
/// hard links keep the old bytes.
pub fn publish_reader(destination: &Path, tag: &str, mut bytes: impl io::Read, claim: Claim) -> Result<()> {
    let replaced = match claim {
        Claim::Replace => std::fs::metadata(destination).ok().filter(|m| m.is_file()),
        Claim::New => None,
    };
    let mut staged = Staged::beside(destination, tag, 0o666)?;
    if let Some(metadata) = replaced {
        staged.file().set_permissions(metadata.permissions())?;
    }
    io::copy(&mut bytes, &mut staged).with_context(|| format!("writing {}", destination.display()))?;
    staged.publish(destination, claim)?;
    Ok(())
}

/// A directory built under a private name beside its destination, then published whole: never
/// merged into an existing directory. Dropped unpublished, it is removed.
pub struct StagedDir {
    path: PathBuf,
}

impl StagedDir {
    pub fn beside(destination: &Path, tag: &str) -> Result<Self> {
        let dir = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = dir.join(format!(".cv-{tag}-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).with_context(|| format!("creating a temporary in {}", dir.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// [`Claim::New`] refuses an existing name. [`Claim::Replace`] swaps the directories in one
    /// step where the platform can (`RENAME_SWAP`, `RENAME_EXCHANGE`); elsewhere it moves the old
    /// one aside first, so the name is briefly absent but never half merged, and a failed claim
    /// puts the old tree back. Every fallback move refuses an existing name, preserving concurrent
    /// claims. If restoration fails, both trees are kept and the error names them.
    /// The replaced tree is then deleted.
    pub fn publish(self, destination: &Path, claim: Claim) -> Result<()> {
        #[cfg(target_vendor = "apple")]
        let swap = |from: &Path, to: &Path| rename_with(from, to, libc::RENAME_SWAP);
        #[cfg(target_os = "linux")]
        let swap = |from: &Path, to: &Path| rename_with(from, to, libc::RENAME_EXCHANGE);
        #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
        let swap = |_: &Path, _: &Path| None;
        self.publish_using(destination, claim, swap, |_, _| Ok(()))
    }

    fn publish_using(
        mut self,
        destination: &Path,
        claim: Claim,
        swap: impl FnOnce(&Path, &Path) -> Option<io::Result<()>>,
        mut before_move: impl FnMut(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        let mut rename = |from: &Path, to: &Path| {
            before_move(from, to)?;
            rename_no_replace(from, to)
        };
        let context = || format!("publishing {}", destination.display());
        let exists = std::fs::symlink_metadata(destination).is_ok();
        let staged = std::mem::take(&mut self.path);
        if claim == Claim::New || !exists {
            let claimed = rename_no_replace(&staged, destination);
            if let Err(e) = claimed {
                self.path = staged;
                if e.kind() == io::ErrorKind::AlreadyExists {
                    return Err(Exists {
                        path: destination.to_owned(),
                        hint: "",
                    }
                    .into());
                }
                return Err(e).with_context(context);
            }
            sync_parent(destination);
            return Ok(());
        }
        let old = match swap(&staged, destination) {
            // The staged name now holds the replaced tree.
            Some(Ok(())) => staged,
            Some(Err(e)) => {
                self.path = staged;
                return Err(e).with_context(context);
            }
            None => {
                let aside = staged.with_extension("replaced");
                if let Err(e) = rename(destination, &aside) {
                    self.path = staged;
                    return Err(e).with_context(context);
                }
                if let Err(e) = rename(&staged, destination) {
                    if let Err(restore) = rename(&aside, destination) {
                        // Neither tree holds the name now. Delete nothing; say where both are.
                        return Err(e).with_context(|| {
                            format!(
                                "publishing {}; putting the replaced tree back also failed ({restore}), \
                                 so it is kept at {} and the new one at {}",
                                destination.display(),
                                aside.display(),
                                staged.display()
                            )
                        });
                    }
                    self.path = staged;
                    return Err(e).with_context(context);
                }
                aside
            }
        };
        sync_parent(destination);
        std::fs::remove_dir_all(&old).with_context(|| format!("removing the replaced {}", old.display()))
    }
}

impl Drop for StagedDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn sync_parent(path: &Path) {
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Permission bits for an output derived from `source`: no more exposed than what it copies.
pub fn mode_of(source: &File) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        source
            .metadata()
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = source;
        0o666
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_claim_fails_closed_without_an_exclusive_rename() {
        let world = cv_testkit::World::new("publish-no-exclusive-rename");
        for directory in [false, true] {
            let from = world.base.join(if directory { "directory" } else { "file" });
            let to = from.with_extension("published");
            if directory {
                std::fs::create_dir(&from).unwrap();
            } else {
                std::fs::write(&from, "complete staged output").unwrap();
            }
            let error = rename_no_replace_using(&from, &to, |_, _| None)
                .expect_err("no exclusive operation means no safe new-name claim");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(from.exists());
            assert!(!to.exists());
        }
    }

    #[test]
    fn move_aside_replacement_preserves_a_concurrent_directory_claim() {
        let world = cv_testkit::World::new("publish-directory-race");
        let destination = world.base.join("tree");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();
        let staged = StagedDir::beside(&destination, "test").unwrap();
        std::fs::write(staged.path().join("new"), "new").unwrap();
        let mut calls = 0;
        let result = staged.publish_using(
            &destination,
            Claim::Replace,
            |_, _| None,
            |_, _| {
                calls += 1;
                if calls == 2 {
                    std::fs::create_dir(&destination)?;
                }
                Ok(())
            },
        );
        assert!(
            result.is_err(),
            "a concurrent claim must block publication and restoration"
        );
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        let kept = std::fs::read_dir(&world.base)
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                (path.join("old").exists() || path.join("new").exists()).then_some(path)
            })
            .count();
        assert_eq!(kept, 2, "both trees must survive a blocked restoration");
    }

    fn staged(dir: &Path, bytes: &[u8]) -> Staged {
        let mut staged = Staged::create(dir, "test", 0o644).unwrap();
        staged.write_all(bytes).unwrap();
        staged
    }

    fn temporaries(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".cv-"))
            .count()
    }

    #[test]
    fn new_refuses_every_existing_name_and_leaves_it_untouched() {
        let world = cv_testkit::World::new("publish-new");
        let dir = &world.base;
        let taken = dir.join("taken");
        std::fs::write(&taken, "before").unwrap();
        let error = staged(dir, b"after").publish(&taken, Claim::New).unwrap_err();
        assert!(error.downcast_ref::<Exists>().is_some(), "{error:#}");
        assert_eq!(std::fs::read(&taken).unwrap(), b"before");
        #[cfg(unix)]
        {
            let dangling = dir.join("dangling");
            std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
            assert!(staged(dir, b"x").publish(&dangling, Claim::New).is_err());
            assert!(!dir.join("nowhere").exists());
        }
        let fresh = dir.join("fresh");
        staged(dir, b"new").publish(&fresh, Claim::New).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        assert_eq!(temporaries(dir), 0);
    }

    /// The path a filesystem without hard links takes (FAT32 on macOS answers EOPNOTSUPP, Linux
    /// vfat EPERM): the no-replace rename claims a free name and refuses a taken one.
    #[test]
    fn without_hard_links_new_falls_back_to_a_rename_that_refuses_to_replace() {
        let world = cv_testkit::World::new("publish-nolink");
        let dir = &world.base;
        for code in [
            #[cfg(unix)]
            libc::EOPNOTSUPP,
            #[cfg(unix)]
            libc::EPERM,
        ] {
            let unsupported = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(code));
            let fresh = dir.join(format!("fresh-{code}"));
            staged(dir, b"new")
                .publish_linking(&fresh, Claim::New, unsupported)
                .unwrap();
            assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
            let error = staged(dir, b"other")
                .publish_linking(&fresh, Claim::New, unsupported)
                .unwrap_err();
            assert!(error.downcast_ref::<Exists>().is_some(), "{error:#}");
            assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        }
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::Unsupported));
        staged(dir, b"kind")
            .publish_linking(&dir.join("kind"), Claim::New, unsupported)
            .unwrap();
        assert_eq!(temporaries(dir), 0);
    }

    #[cfg(unix)]
    #[test]
    fn replace_moves_the_name_and_a_reader_keeps_the_old_bytes() {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let world = cv_testkit::World::new("publish-replace");
        let dir = &world.base;
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::write(&target, "before").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut reader = File::open(&target).unwrap();
        let old = reader.metadata().unwrap().ino();
        staged(dir, b"after!").publish(&target, Claim::Replace).unwrap();
        assert_ne!(std::fs::metadata(&target).unwrap().ino(), old);
        assert_eq!(std::fs::read(&target).unwrap(), b"after!");
        let mut held = String::new();
        reader.read_to_string(&mut held).unwrap();
        assert_eq!(held, "before");
        staged(dir, b"via link").publish(&link, Claim::Replace).unwrap();
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"after!");
        assert_eq!(temporaries(dir), 0);
    }

    /// Without an atomic swap the old tree is moved aside before the new one takes the name. When
    /// that second rename fails the old tree goes back; when putting it back fails too, nothing is
    /// deleted and the error says where both trees are.
    #[test]
    fn a_failed_move_aside_replacement_keeps_a_tree_and_reports_the_error() {
        let world = cv_testkit::World::new("publish-move-aside");
        let tree = |path: &Path| std::fs::read_to_string(path.join("which")).ok();
        // Each case fails the renames whose (1-based) numbers it lists: 1 moves the old tree
        // aside, 2 claims the name, 3 puts the old tree back.
        for (case, failing, published) in [
            ("ok", &[][..], Some("new")),
            ("aside", &[1][..], Some("old")),
            ("claim", &[2][..], Some("old")),
            ("restore", &[2, 3][..], None),
        ] {
            let dir = world.base.join(case);
            std::fs::create_dir(&dir).unwrap();
            let destination = dir.join("tree");
            std::fs::create_dir(&destination).unwrap();
            std::fs::write(destination.join("which"), "old").unwrap();
            let staged = StagedDir::beside(&destination, "test").unwrap();
            std::fs::write(staged.path().join("which"), "new").unwrap();
            let mut calls = 0;
            let rename = |_: &Path, _: &Path| {
                calls += 1;
                match failing.contains(&calls) {
                    true => Err(io::Error::other(format!("injected failure {calls}"))),
                    false => Ok(()),
                }
            };
            let result = staged.publish_using(&destination, Claim::Replace, |_, _| None, rename);
            assert_eq!(result.is_ok(), failing.is_empty(), "{case}: {result:?}");
            assert_eq!(tree(&destination).as_deref(), published, "{case}");
            let left: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p != &destination)
                .collect();
            if published.is_some() {
                assert!(left.is_empty(), "{case}: {left:?}");
                continue;
            }
            // Neither tree holds the name: both are kept, and the error names where.
            let error = format!("{:#}", result.unwrap_err());
            let mut kept: Vec<_> = left.iter().filter_map(|p| tree(p)).collect();
            kept.sort();
            assert_eq!(kept, ["new", "old"], "{case}: {left:?}");
            for path in &left {
                assert!(error.contains(&path.display().to_string()), "{case}: {error}");
            }
            assert!(
                error.contains("injected failure 2") && error.contains("injected failure 3"),
                "{error}"
            );
        }
    }

    /// Another writer replaces the published name after retract has looked at it: retract still
    /// removes nothing but its own file.
    #[cfg(unix)]
    #[test]
    fn retract_never_removes_a_file_that_replaced_its_own() {
        let world = cv_testkit::World::new("publish-retract-race");
        let path = world.base.join("out");
        let file = staged(&world.base, b"ours").publish(&path, Claim::New).unwrap();
        let replace = || {
            staged(&world.base, b"theirs")
                .publish(&path, Claim::Replace)
                .map(drop)
                .unwrap()
        };
        retract_racing(&path, &file, replace).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
        assert_eq!(temporaries(&world.base), 0);
    }

    #[cfg(unix)]
    #[test]
    fn mode_is_kept_and_retract_removes_only_its_own_inode() {
        use std::os::unix::fs::PermissionsExt;
        let world = cv_testkit::World::new("publish-mode");
        let dir = &world.base;
        let path = dir.join("out");
        let mut private = Staged::create(dir, "test", 0o640).unwrap();
        private.write_all(b"x").unwrap();
        let file = private.publish(&path, Claim::New).unwrap();
        let umask_applied = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(umask_applied & !0o640, 0, "{umask_applied:o}");
        assert_eq!(mode_of(&file), umask_applied);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "someone else").unwrap();
        let cause = || anyhow::anyhow!("companion failed");
        assert_eq!(format!("{:#}", retract(&path, &file, cause())), "companion failed");
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else");
        std::fs::remove_file(&path).unwrap();
        let file = staged(dir, b"ours").publish(&path, Claim::New).unwrap();
        assert_eq!(format!("{:#}", retract(&path, &file, cause())), "companion failed");
        assert!(!path.exists());
        assert_eq!(temporaries(dir), 0);
    }
}
