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
        if expected == actual {
            return Ok(());
        }
        panic!(
            "{} differs from the frozen emitted output (CV_REGEN_CORPUS=1 rewrites){}",
            self.1,
            Self::diff(&expected, &actual)
        );
    }

    /// Name the entries that moved and show both sides when they are small enough to read.
    /// A `[len, sha]` entry then at least reports its length; a full entry reads as the field
    /// diff it is.
    fn diff(expected: &str, actual: &str) -> String {
        let parse = |text: &str| {
            serde_json::from_str::<serde_json::Map<String, Value>>(text)
                .unwrap_or_else(|e| panic!("frozen golden is not an object: {e}"))
        };
        let (expected, actual) = (parse(expected), parse(actual));
        let mut report = String::new();
        for label in expected
            .keys()
            .chain(actual.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if expected.get(label) == actual.get(label) {
                continue;
            }
            let side = |value: Option<&Value>| match value {
                Some(value) => {
                    let line = serde_json::to_string(value).unwrap_or_default();
                    if line.len() <= 400 {
                        line
                    } else {
                        format!("{}… ({} bytes)", &line[..200], line.len())
                    }
                }
                None => "<absent>".into(),
            };
            report.push_str(&format!(
                "\n  {label}:\n    frozen: {}\n    actual: {}",
                side(expected.get(label)),
                side(actual.get(label))
            ));
        }
        report
    }
}
