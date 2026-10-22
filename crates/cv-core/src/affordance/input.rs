//! Fallible projections of tool arguments, shared by affordance and event reducers.
use crate::json::JsonView;
use crate::projection::string_until;
use anyhow::Result;

/// Enough decoded text to reproduce `truncate(text, cap)`, across arbitrary chunk boundaries.
/// Materialize one demanded string, never its sibling argument fields or a Value tree.
pub(crate) fn string(view: JsonView<'_>) -> Result<Option<String>> {
    let mut out = String::new();
    Ok(view
        .for_each_str_chunk(|chunk| {
            out.push_str(chunk);
            Ok(())
        })?
        .then_some(out))
}

pub(crate) fn field_string(view: JsonView<'_>, key: &str) -> Result<Option<String>> {
    view.get(key)?.map(string).transpose().map(Option::flatten)
}

pub(crate) fn prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    crate::projection::Head::of_string(view, cap)
}

pub(crate) fn scalar_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    if let Some(text) = prefix(view, cap)? {
        return Ok(Some(text));
    }
    Ok(view
        .as_number()?
        .and_then(|number| number.as_u64())
        .map(|number| number.to_string()))
}

/// Reject all-whitespace strings even when `trim` is false; otherwise preserve whitespace unless
/// trimming was requested. Scan before allocating. Trimming uses decoded
/// byte bounds; a huge ignored whitespace tail does not become a transient String allocation.
pub(crate) fn item_string(view: JsonView<'_>, trim: bool) -> Result<Option<String>> {
    let mut offset = 0;
    let mut start = None;
    let mut end = 0;
    if !view.for_each_str_chunk(|chunk| {
        for (at, ch) in chunk.char_indices() {
            if !ch.is_whitespace() {
                start.get_or_insert(offset + at);
                end = offset + at + ch.len_utf8();
            }
        }
        offset += chunk.len();
        Ok(())
    })? {
        return Ok(None);
    }
    let Some(start) = start else {
        return Ok(None);
    };
    if !trim {
        return string(view);
    }
    let mut out = String::new();
    offset = 0;
    view.for_each_str_chunk(|chunk| {
        let lo = start.saturating_sub(offset).min(chunk.len());
        let hi = end.saturating_sub(offset).min(chunk.len());
        if lo < hi {
            out.push_str(&chunk[lo..hi]);
        }
        offset += chunk.len();
        Ok(())
    })?;
    Ok(Some(out))
}

/// Exact membership without allocating an arbitrarily long action/status string.
pub(crate) fn word(view: JsonView<'_>, words: &[&str]) -> Result<Option<usize>> {
    let cap = words.iter().map(|word| word.len()).max().unwrap_or(0);
    let mut text = String::new();
    let mut bytes = 0;
    if !string_until(view, |chunk| {
        bytes += chunk.len();
        if text.len() <= cap {
            for ch in chunk.chars() {
                text.push(ch);
                if text.len() > cap {
                    break;
                }
            }
        }
        bytes > cap
    })? {
        return Ok(None);
    }
    Ok(words.iter().position(|word| word.len() == bytes && *word == text))
}

/// The trimmed [`Head`](crate::projection::Head) of a string: `truncate(trim(text), cap)`.
pub(crate) fn trimmed_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    let mut head = crate::projection::Head::trimmed(cap);
    let is_string = string_until(view, |chunk| head.push(chunk))?;
    Ok(is_string.then(|| head.finish()))
}

/// The untrimmed prefix of a nonblank string, for object item text whose whitespace is retained.
pub(crate) fn nonblank_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    let mut out = crate::projection::Head::bounded(cap);
    let mut nonblank = false;
    let is_string = string_until(view, |chunk| {
        out.push(chunk);
        nonblank |= !chunk.trim().is_empty();
        out.full() && nonblank
    })?;
    Ok((is_string && nonblank).then(|| out.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Json;

    #[test]
    fn source_prefixes_preserve_trim_flatten_and_unicode_boundaries() {
        let path = std::env::temp_dir().join(format!("cv-argument-prefix-{}", uuid::Uuid::new_v4()));
        for text in [
            format!("{}short{}", "\u{2003}".repeat(40_000), "\n".repeat(80_000)),
            format!("{}{}", "λ\n".repeat(61), " ".repeat(80_000)),
            format!("{}late", " ".repeat(80_000)),
            "\n\u{2003}".repeat(40_000),
            "create".to_string(),
            "create\n".to_string(),
        ] {
            let raw = serde_json::to_vec(&text).unwrap();
            std::fs::write(&path, &raw).unwrap();
            let source = Json::source(&path, 0, raw.len() as u64).unwrap();
            source
                .with_view(|view| {
                    assert_eq!(prefix(view, 120)?, Some(crate::ir::truncate(&text, 120)));
                    assert_eq!(trimmed_prefix(view, 120)?, Some(crate::ir::truncate(text.trim(), 120)));
                    assert_eq!(
                        nonblank_prefix(view, 120)?,
                        (!text.trim().is_empty()).then(|| crate::ir::truncate(&text, 120))
                    );
                    assert_eq!(
                        word(view, &["create", "delete"])?,
                        ["create", "delete"].iter().position(|word| *word == text)
                    );
                    Ok(())
                })
                .unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }
}
