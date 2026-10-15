//! Bounded JSONL staging: serialize successfully before opening the destination.
use super::bytes::ByteStage;
use crate::publish::Claim;
use anyhow::Result;
use serde::Serialize;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

pub(crate) struct JsonlStage {
    output: BufWriter<ByteStage>,
    records: usize,
    failed: bool,
}
impl JsonlStage {
    pub(crate) fn new() -> Self {
        Self {
            output: BufWriter::with_capacity(64 * 1024, ByteStage::new()),
            records: 0,
            failed: false,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.records
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.records == 0
    }
    pub(crate) fn push(&mut self, value: impl Serialize) -> Result<()> {
        self.push_with(|writer| serde_json::to_writer(writer, &value).map_err(Into::into))
    }
    pub(crate) fn push_with(&mut self, produce: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        anyhow::ensure!(!self.failed, "JSONL stage already failed");
        self.failed = true;
        produce(&mut self.output)?;
        self.output.write_all(b"\n")?;
        self.records += 1;
        self.failed = false;
        Ok(())
    }
    pub(crate) fn into_source(self) -> Result<super::bytes::ByteSource> {
        anyhow::ensure!(!self.failed, "cannot consume a failed JSONL stage");
        Ok(self.output.into_inner().map_err(|e| e.into_error())?.finish()?)
    }
    pub(crate) fn publish(self, path: &Path, claim: Claim) -> Result<()> {
        anyhow::ensure!(!self.failed, "cannot publish a failed JSONL stage");
        let stage = self.output.into_inner().map_err(|e| e.into_error())?;
        publish_stage(stage, path, claim)
    }
}

/// Publish fully produced bytes through [`crate::publish`]: never written into an existing file.
/// Checked serialization must complete before calling.
pub(crate) fn publish_stage(stage: ByteStage, path: &Path, claim: Claim) -> Result<()> {
    let source = stage.finish()?;
    let reader = BufReader::with_capacity(256 * 1024, source.reader()?);
    crate::publish::publish_reader(path, "stage", reader, claim)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_spilled_jsonl_and_failed_publication() -> Result<()> {
        let dir = cv_testkit::World::new("jsonl-stage-spill");
        let path = dir.base.join("records.jsonl");
        let value = serde_json::json!({"escaped": "a\"\\\nλ".repeat(300_000), "n": 1.25});
        let mut stage = JsonlStage::new();
        stage.push(&value)?;
        stage.push(serde_json::Value::Null)?;
        stage.publish(&path, Claim::New)?;
        assert_eq!(
            std::fs::read(&path)?,
            format!("{}\nnull\n", serde_json::to_string(&value)?).as_bytes()
        );
        struct Failure;
        impl Serialize for Failure {
            fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic failure"))
            }
        }
        let before = std::fs::read(&path)?;
        let mut stage = JsonlStage::new();
        stage.push(serde_json::json!({"valid": true}))?;
        assert!(stage.push(Failure).is_err());
        assert!(stage.publish(&path, Claim::Replace).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        let mut stage = JsonlStage::new();
        assert!(stage
            .push_with(|writer| {
                writer.write_all(b"{\"partial\":")?;
                anyhow::bail!("checked payload source failed")
            })
            .is_err());
        assert!(stage.publish(&path, Claim::Replace).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }
    /// A replaced name gets a new file: the old inode, its other hard links and a symlink's
    /// target keep their bytes; the replaced file's permissions carry over; New refuses.
    #[cfg(unix)]
    #[test]
    fn replaces_the_name_never_the_linked_file() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = cv_testkit::World::new("jsonl-stage-links");
        let path = dir.base.join("original");
        let hard = dir.base.join("hard");
        let sym = dir.base.join("sym");
        std::fs::write(&path, "before")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
        std::fs::hard_link(&path, &hard)?;
        symlink(&path, &sym)?;
        let staged = || -> Result<JsonlStage> {
            let mut stage = JsonlStage::new();
            stage.push(42)?;
            Ok(stage)
        };
        let error = staged()?.publish(&sym, Claim::New).unwrap_err();
        assert!(error.downcast_ref::<crate::publish::Exists>().is_some(), "{error:#}");
        staged()?.publish(&sym, Claim::Replace)?;
        assert_eq!(std::fs::read(&sym)?, b"42\n");
        assert!(!std::fs::symlink_metadata(&sym)?.file_type().is_symlink());
        assert_eq!(std::fs::metadata(&sym)?.permissions().mode() & 0o777, 0o640);
        staged()?.publish(&path, Claim::Replace)?;
        assert_eq!(std::fs::read(&path)?, b"42\n");
        assert_eq!(std::fs::read(&hard)?, b"before");
        Ok(())
    }
}
