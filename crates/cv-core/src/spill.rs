//! Operation-owned, bounded temporary storage for ordered records and exact string membership.
mod exact;
mod ordered;
pub(crate) use exact::ExactStrings;
pub(crate) use ordered::{Ordered, Record};

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::path::PathBuf;

/// Exclusively-created temporary file. On Unix it is unlinked immediately; other platforms
/// remove the owned path after the caller closes its file handles.
struct Temporary {
    path: PathBuf,
    linked: bool,
}
impl Temporary {
    fn create(label: &str) -> Result<(File, Self)> {
        let path = std::env::temp_dir().join(format!("cv-{label}-{}", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).context("creating private spill file")?;
        let owner = Self { path, linked: true };
        #[cfg(unix)]
        let owner = {
            let mut owner = owner;
            fs::remove_file(&owner.path).context("unlinking private spill file")?;
            owner.linked = false;
            owner
        };
        Ok((file, owner))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        // Only the exclusive path this operation created, never a source or another pass's file.
        if self.linked {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn dropping_unlinked_temporary_preserves_reused_path() {
        let (file, owner) = Temporary::create("path-reuse-test").unwrap();
        let path = owner.path.clone();
        fs::write(&path, b"different owner").unwrap();
        drop(file);
        drop(owner);
        assert_eq!(fs::read(&path).unwrap(), b"different owner");
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn private_file_permissions_and_owned_cleanup() {
        let (file, owner) = Temporary::create("ownership-test").unwrap();
        let path = owner.path.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert!(!path.exists());
        }
        drop(file);
        drop(owner);
        assert!(!path.exists());
    }
}
