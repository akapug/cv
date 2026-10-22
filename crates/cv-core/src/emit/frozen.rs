//! The recorded-output oracle: a test names a golden under `tests/fixtures/emit-frozen/`, inserts
//! labelled entries, and `check` compares them with the file (shared by every emitter and
//! distill test and by the readers whose retired oracles it froze: Goose legacy discovery,
//! Claude fork heads; token stats froze its own in the purge lane).
use anyhow::Result;
use serde_json::Value;
use std::{fs, path::Path};

/// Output goldens per test under `tests/fixtures/emit-frozen/`, frozen from retired oracles
/// (the pre-streaming emitters, distill's pre-sharing products, the reader oracles above).
/// `CV_REGEN_CORPUS=1` rewrites them; explain every diff in the commit.
pub(crate) struct Frozen(pub(crate) std::collections::BTreeMap<String, Value>, &'static str);

impl Frozen {
    pub(crate) fn new(name: &'static str) -> Self {
        Self(Default::default(), name)
    }

    /// Record one labelled entry; labels are unique per golden.
    pub(crate) fn insert(&mut self, label: impl Into<String>, entry: Value) {
        assert!(self.0.insert(label.into(), entry).is_none(), "unique label");
    }

    pub(crate) fn check(self) -> Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/emit-frozen/{}.json", self.1));
        let rows: Vec<String> = (self.0.iter())
            .map(|(label, entry)| {
                Ok(format!(
                    "{}: {}",
                    serde_json::to_string(label)?,
                    serde_json::to_string(entry)?
                ))
            })
            .collect::<Result<_>>()?;
        let actual = format!("{{\n{}\n}}\n", rows.join(",\n"));
        if std::env::var_os("CV_REGEN_CORPUS").is_some() {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, &actual)?;
        }
        let expected = anyhow::Context::with_context(fs::read_to_string(&path), || {
            format!("{} (CV_REGEN_CORPUS=1 writes it)", path.display())
        })?;
        assert!(
            expected == actual,
            "{} differs from the frozen emitted output (CV_REGEN_CORPUS=1 rewrites)",
            self.1
        );
        Ok(())
    }
}
