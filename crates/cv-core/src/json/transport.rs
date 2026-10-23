//! Operation-private source descriptors. A staged descriptor keeps the original accepted source
//! identity; restoration never captures or validates a newer file in place of that source.
use super::{Json, JsonSource};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub(crate) struct SourceDescriptor {
    source: usize,
    offset: u64,
    len: u64,
    decoded: Option<usize>,
}

/// Structural state scales with distinct captured source identities and decoded arenas.
/// One append arena is shared by all encoded arguments from a capture and survives transport.
/// Different captured identities of a single path are deliberately separate accepted sources.
#[derive(Default)]
pub(crate) struct SourceRegistry {
    ids: HashMap<JsonSource, usize>,
    captured_ids: HashMap<usize, usize>,
    sources: Vec<JsonSource>,
    decoded_ids: HashMap<usize, usize>,
    decoded: Vec<std::sync::Arc<crate::harness::store::bytes::ByteSource>>,
}
impl SourceRegistry {
    #[cfg(test)]
    pub(crate) fn owner_counts(&self) -> (usize, usize) {
        (self.sources.len(), self.decoded.len())
    }

    /// Replace a source value with an explicit transport placeholder without resolving its bytes.
    pub(crate) fn detach(&mut self, value: &mut Json) -> Option<SourceDescriptor> {
        let Json::Source(range) = value else { return None };
        // Repeated descriptors from one capture share its Arc. Retained source owners keep
        // pointer keys alive; independent equivalent captures still use structural equality.
        let captured = std::sync::Arc::as_ptr(&range.source.0) as usize;
        let source = if let Some(id) = self.captured_ids.get(&captured) {
            *id
        } else {
            let id = *self.ids.entry(range.source.clone()).or_insert_with(|| {
                let id = self.sources.len();
                self.sources.push(range.source.clone());
                id
            });
            // A structurally equivalent independent capture need not be retained. Cache only
            // the Arc actually owned by the registry, so allocator pointer reuse stays safe.
            if std::sync::Arc::ptr_eq(&range.source.0, &self.sources[id].0) {
                self.captured_ids.insert(captured, id);
            }
            id
        };
        let decoded = range.decoded.as_ref().map(|bytes| {
            let key = std::sync::Arc::as_ptr(bytes) as usize;
            *self.decoded_ids.entry(key).or_insert_with(|| {
                let id = self.decoded.len();
                self.decoded.push(bytes.clone());
                id
            })
        });
        let descriptor = SourceDescriptor {
            source,
            offset: range.offset,
            len: range.len,
            decoded,
        };
        *value = Json::Owned(serde_json::Value::Null);
        Some(descriptor)
    }

    /// Bounds-check against the original accepted source. Demanded reads later validate that
    /// identity normally; excluded records need not read or stat these unused payloads.
    pub(crate) fn restore(&self, descriptor: SourceDescriptor) -> Result<Json> {
        let source = self
            .sources
            .get(descriptor.source)
            .context("unknown staged JSON source")?;
        let range = match descriptor.decoded {
            None => source.range(descriptor.offset, descriptor.len)?,
            Some(id) => {
                let bytes = self.decoded.get(id).context("unknown staged decoded JSON")?.clone();
                let len = bytes.byte_len()?;
                // A descriptor must name a complete JSON value; even an empty JSON string
                // occupies two quote bytes, so a zero-length range cannot be valid.
                anyhow::ensure!(
                    descriptor.len != 0
                        && descriptor
                            .offset
                            .checked_add(descriptor.len)
                            .is_some_and(|end| end <= len),
                    "staged decoded JSON out of bounds"
                );
                super::JsonRange {
                    source: source.clone(),
                    offset: descriptor.offset,
                    len: descriptor.len,
                    decoded: Some(bytes),
                }
            }
        };
        Ok(Json::Source(std::sync::Arc::new(range)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_guards_do_not_collapse_different_wal_states_or_get_recaptured() {
        let world = crate::test_world("json-source-read-guard");
        let path = world.base.join("source.json");
        let wal = world.base.join("source.json-wal");
        std::fs::write(&path, r#"{"value":"old"}"#).unwrap();
        let mut reads = crate::source::ReadSet::default();
        reads.record(&wal);
        let before = JsonSource::capture_with_reads(&path, &reads).unwrap();
        let mut value = before
            .read(ReadInput::Document, Validation::Value, |view| view.to_json())
            .unwrap();
        let mut registry = SourceRegistry::default();
        let descriptor = registry.detach(&mut value).unwrap();
        std::fs::write(&wal, b"new WAL").unwrap();
        let mut reads = crate::source::ReadSet::default();
        reads.record(&wal);
        let after = JsonSource::capture_with_reads(&path, &reads).unwrap();
        let mut value = after
            .read(ReadInput::Document, Validation::Value, |view| view.to_json())
            .unwrap();
        registry.detach(&mut value).unwrap();
        assert_eq!(registry.sources.len(), 2);
        assert_ne!(before, after);
        assert!(registry.restore(descriptor).unwrap().canonical_len().is_err());
        let value = after
            .read(ReadInput::Document, Validation::Value, |view| view.to_json())
            .unwrap();
        let error = value
            .with_view::<()>(|_| {
                std::fs::remove_file(&wal)?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("dependencies changed"));
    }

    #[test]
    fn decoded_documents_register_one_arena_and_original_identity() {
        let world = crate::test_world("decoded-source-transport");
        let path = world.base.join("source.json");
        let argument = format!("{{\"p\":\"{}\"}}", "x".repeat(8192));
        std::fs::write(&path, serde_json::to_vec(&vec![&argument; 32]).unwrap()).unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let mut registry = SourceRegistry::default();
        let descriptors = source
            .read(ReadInput::Document, Validation::Value, |view| {
                view.array()?
                    .unwrap()
                    .map(|view| {
                        let mut value = view?.encoded_argument()?;
                        Ok(registry.detach(&mut value).unwrap())
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(registry.sources.len(), 1);
        assert_eq!(registry.decoded.len(), 1);
        for descriptor in descriptors {
            let restored = registry.restore(descriptor).unwrap();
            assert_eq!(restored.canonical_len().unwrap(), argument.len() as u64);
        }
        std::fs::remove_file(&path).unwrap();
        assert!(registry.sources[0].check().is_err());
    }

    #[test]
    fn repeated_sources_share_original_identity_and_never_recapture() {
        let world = crate::test_world("json-source-transport");
        let path = world.base.join("source.json");
        std::fs::write(&path, r#"{"value":"old"}"#).unwrap();
        let mut first = Json::source(&path, 0, 15).unwrap();
        let mut second = Json::source(&path, 0, 15).unwrap();
        let mut registry = SourceRegistry::default();
        let descriptor = registry.detach(&mut first).unwrap();
        registry.detach(&mut second).unwrap();
        assert_eq!(registry.sources.len(), 1);
        assert_eq!(
            registry.captured_ids.len(),
            1,
            "only retained captures acquire fast keys"
        );
        let mut repeated = Json::Source(std::sync::Arc::new(registry.sources[0].range(0, 15).unwrap()));
        let mut cloned = repeated.clone();
        registry.detach(&mut repeated).unwrap();
        let captures = registry.captured_ids.len();
        registry.detach(&mut cloned).unwrap();
        assert_eq!(
            registry.captured_ids.len(),
            captures,
            "cloned source descriptor reuses its capture key"
        );
        assert_eq!(registry.sources.len(), 1);
        std::fs::write(&path, r#"{"value":"new"}"#).unwrap();
        let restored = registry.restore(descriptor).unwrap();
        assert!(restored.resolve().is_err());
        assert!(matches!(first, Json::Owned(serde_json::Value::Null)));
    }
}
