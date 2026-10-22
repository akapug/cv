//! Selected payload retrieval from a prune sidecar.

use super::*;

/// Retrieve a stashed original by its sidecar id: a tool_use_id, `<id>#tur`, `<id>#tr{index}`,
/// `<id>#think{index}`, `<id>#declass{index}`, or `<id>#declass` (minted by prune). Returns the
/// stored JSON value (string, content-block array incl. images, or raw toolUseResult object). Errors
/// if the id matches more than one entry — a duplicated id means the sidecar can't say which payload
/// is which, and silently returning one of them would misattribute data.
pub fn retrieve(sidecar_path: &Path, id: &str) -> Result<Value> {
    let source = crate::json::JsonSource::capture(sidecar_path)
        .with_context(|| format!("reading sidecar {}", sidecar_path.display()))?;
    let file =
        std::fs::File::open(sidecar_path).with_context(|| format!("reading sidecar {}", sidecar_path.display()))?;
    retrieve_captured(&source, &file, sidecar_path, id)
}

pub(super) fn retrieve_captured(
    source: &crate::json::JsonSource,
    file: &std::fs::File,
    path: &Path,
    id: &str,
) -> Result<Value> {
    let result = (|| {
        source.check_metadata(file.metadata()?)?;
        let extent = Range {
            offset: 0,
            len: source.len(),
        };
        let mut available = Vec::new();
        let mut found = None;
        let mut duplicates = 0usize;
        let mut reader = crate::harness::store::jsonl::Records::new(source, file);
        for row in ranges::records(file, extent)? {
            let (row, _) = row?;
            if row.len == 0 {
                continue;
            }
            // Unknown fields keep derived serde's IgnoredAny acceptance; the entry's Value fields
            // are checked by the projection.
            let projected = reader.read_with(row.offset, row.len, crate::json::Validation::Syntax, |view| {
                project_sidecar_entry(view, id, available.len() < 20)
            });
            // The validator rejected the row: invalid UTF-8 fails the whole sidecar under its own
            // name, as in prune's walk; any other malformed entry is skipped.
            if projected
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<crate::json::InvalidJson>().is_some())
                && !ranges::valid_utf8(&mut ranges::reader(file, row)?)?
            {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "stream did not contain valid UTF-8").into(),
                );
            }
            accept_sidecar_entry(projected, &mut available, &mut found, &mut duplicates)?;
        }
        if duplicates > 0 {
            bail!(
                "id {id:?} appears {} times in sidecar {} — ambiguous, refusing to guess which payload you meant",
                duplicates + 1,
                path.display()
            );
        }
        let Some(content) = found else {
            let mut message = format!("id {id:?} not found in sidecar. available: ");
            for (i, candidate) in available.iter().enumerate() {
                if i != 0 {
                    message.push_str(", ");
                }
                candidate.with_view(|view| {
                    view.for_each_str_chunk(|part| {
                        message.push_str(part);
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            bail!(message);
        };
        // Only the unambiguous requested output becomes owned, after scanning every row.
        Ok(content.resolve()?.into_owned())
    })();
    let result = result.map_err(|error: anyhow::Error| {
        if error.downcast_ref::<std::io::Error>().is_some() {
            error.context(format!("reading sidecar {}", path.display()))
        } else {
            error
        }
    });
    // Guard failures take precedence even when a malformed row or callback stopped the scan.
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    result
}

fn accept_sidecar_entry(
    projected: Result<(Option<crate::Json>, Option<crate::Json>)>,
    available: &mut Vec<crate::Json>,
    found: &mut Option<crate::Json>,
    duplicates: &mut usize,
) -> Result<()> {
    let (shown_id, content) = match projected {
        Ok(entry) => entry,
        Err(error)
            if error.downcast_ref::<crate::json::InvalidJson>().is_some()
                || error
                    .downcast_ref::<crate::harness::store::document::InvalidStruct>()
                    .is_some() =>
        {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    if let Some(id) = shown_id {
        available.push(id);
    }
    if let Some(content) = content {
        if found.is_some() {
            *duplicates += 1;
        } else {
            *found = Some(content);
        }
    }
    Ok(())
}

fn project_sidecar_entry(
    view: crate::json::JsonView<'_>,
    id: &str,
    keep_id: bool,
) -> Result<(Option<crate::Json>, Option<crate::Json>)> {
    use crate::harness::store::document::{Fields, InvalidStruct};
    const KEYS: &[&str] = &["id", "slot", "name", "input", "content", "size", "line_count", "kind"];
    let fields = Fields::parse_struct(view, KEYS)?.ok_or(InvalidStruct("expected sidecar entry"))?;
    let field = |key| fields.get(key).ok_or(InvalidStruct("missing sidecar field"));
    // Unknown fields retain derived serde's IgnoredAny semantics. Only Value fields receive
    // strict Value acceptance, with the enclosing struct's consumed recursion level.
    for key in ["input", "content"] {
        if !field(key)?.accepts_value_at_depth(1)? {
            return Err(InvalidStruct("invalid sidecar value").into());
        }
    }
    for key in ["slot", "name", "kind", "id"] {
        let value = field(key)?;
        if !value.is_string()? || !value.accepts_value_at_depth(1)? {
            return Err(InvalidStruct("invalid sidecar string").into());
        }
    }
    for key in ["size", "line_count"] {
        let value = field(key)?;
        if value.is_string()? || value.is_object()? || value.is_array()? {
            return Err(InvalidStruct("invalid sidecar integer").into());
        }
        if !value.accepts_value_at_depth(1)? {
            return Err(InvalidStruct("invalid sidecar integer").into());
        }
        let number = value.resolve()?;
        if number.as_u64().and_then(|n| usize::try_from(n).ok()).is_none() {
            return Err(InvalidStruct("invalid sidecar integer").into());
        }
    }
    let shown = if keep_id { Some(field("id")?.to_json()?) } else { None };
    let mut at = 0usize;
    let mut matches = true;
    field("id")?.for_each_str_chunk(|part| {
        if matches {
            matches = at.checked_add(part.len()).and_then(|end| id.as_bytes().get(at..end)) == Some(part.as_bytes());
            if matches {
                at += part.len();
            }
        }
        Ok(())
    })?;
    let content = if matches && at == id.len() {
        Some(field("content")?.to_json()?)
    } else {
        None
    };
    Ok((shown, content))
}
