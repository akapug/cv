//! Bounded decoded string projections. Source views are already validated; the enclosing
//! Json::with_view checks identity after callbacks, including deliberately completed projections.
use crate::json::JsonView;
use anyhow::Result;

/// Visit decoded chunks until `fold` returns true. Returns false only for a nonstring. Only the
/// [`Stop`](crate::stream::Stop) it raises is intercepted; source/decoder failures remain errors. Callers keep
/// reducer state staged until the enclosing with_view succeeds. The bool means "was a string",
/// including early stops; callers track completion in their own reducer state.
pub(crate) fn string_until(view: JsonView<'_>, mut fold: impl FnMut(&str) -> bool) -> Result<bool> {
    let stop = |chunk: &str| {
        if fold(chunk) {
            Err(crate::stream::Stop.into())
        } else {
            Ok(())
        }
    };
    Ok(crate::stream::stopped(view.for_each_str_chunk(stop))?.unwrap_or(true))
}

/// At most `max_chars` decoded Unicode characters, with original whitespace/newlines and no
/// ellipsis. Labels can request cap+1 and apply the shared IR truncation once after composition.
pub(crate) fn string_prefix(view: JsonView<'_>, max_chars: usize) -> Result<Option<String>> {
    if !view.is_string()? {
        return Ok(None);
    }
    let mut text = String::new();
    let mut chars = 0;
    if max_chars > 0 {
        string_until(view, |chunk| {
            for ch in chunk.chars().take(max_chars - chars) {
                text.push(ch);
                chars += 1;
            }
            chars == max_chars
        })?;
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Json;

    #[test]
    fn validated_escaped_source_stops_and_preserves_outer_identity_check() {
        let path = std::env::temp_dir().join(format!("cv-projection-stop-{}", uuid::Uuid::new_v4()));
        let replacement = path.with_extension("replacement");
        let text = "λ\n".repeat(100_000);
        let raw = serde_json::to_vec(&text).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let source = Json::source(&path, 0, raw.len() as u64).unwrap();
        source
            .with_view(|view| {
                let mut callbacks = 0;
                assert!(string_until(view, |_| {
                    callbacks += 1;
                    true
                })?);
                assert_eq!(callbacks, 1, "validated decoder must stop after the determined answer");
                for cap in [0, 1, 81, 121] {
                    assert_eq!(string_prefix(view, cap)?, Some(text.chars().take(cap).collect()));
                }
                Ok(())
            })
            .unwrap();
        std::fs::write(&replacement, &raw).unwrap();
        assert!(
            source
                .with_view(|view| string_until(view, |_| {
                    std::fs::rename(&replacement, &path).unwrap();
                    true
                }))
                .is_err(),
            "early completion must not conceal source replacement"
        );
        std::fs::remove_file(path).unwrap();
        assert!(source.with_view(|view| string_prefix(view, 81)).is_err());
    }
}
