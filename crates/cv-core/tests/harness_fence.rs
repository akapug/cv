//! The adapter-boundary fence (the `dependency_fence.rs` shape): a shared module names a harness
//! only inside an exhaustive `match`. Everywhere else the fact belongs to the `Adapter` (a method
//! with a default), so a `Harness::<Name>` literal in product code outside cv-core's
//! `src/harness/` and `src/formats.rs` is a leak, and any one fails this test. The binary (`cv`)
//! and the search index (`cv-search`) are read by the same rule, with no adapter directory.
//!
//! The scan is textual and approximate, on purpose: `#[cfg(test)]`/`#[test]` items are skipped to
//! their closing brace (or the `;` or `,` that ends a braceless one), `tests.rs`/`*_tests.rs`
//! files and `tests/` directories are not read, comments and string (raw and multi-line ones
//! included) and char literals are masked by a small lexer before braces are counted, and a
//! `match` is exhaustive when the literals inside its brace-matched body name every variant of
//! [`Harness::ALL`] (a `_` arm beside a partial list does not count). A file's `use Harness as H`
//! (or `type H = Harness`) makes `H::Name` a literal too, and importing the variants themselves
//! (`Harness::*`, `Harness::{…}`) is a breach, since bare variant names cannot be told from any
//! other. Rust's own exhaustiveness checking is the real judge; this fence only has to tell a
//! per-harness table from a special case.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use cv_core::Harness;
use regex::Regex;

/// The product sources the fence reads, each with whether it holds the adapters (cv-core's): the
/// two crates above it consume them (the binary and the search index). Each is required: a missing
/// one is a wrong root, never a pass.
fn source_roots() -> Vec<(PathBuf, bool)> {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let roots = vec![
        (core.join("src"), true),
        (core.join("../cv/src"), false),
        (core.join("../cv-search/src"), false),
    ];
    for (root, _) in &roots {
        assert!(root.is_dir(), "the fence root {} is missing", root.display());
    }
    roots
}

/// Every `.rs` under a source root the fence reads: not a test file, and in the adapters' crate
/// not an adapter or the format manifests.
fn fenced_files(dir: &Path, adapters: bool, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap();
        if path.is_dir() {
            if !(adapters && name == "harness") && name != "tests" {
                fenced_files(&path, adapters, out);
            }
        } else if name.ends_with(".rs")
            && !(adapters && name == "formats.rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
        {
            out.push(path);
        }
    }
}

/// The file's lines with comments, string and char literals blanked (newlines kept), so braces
/// and `match` are read from code only. A small lexer, not a per-line regex: a raw string or a
/// block comment that spans lines must not unbalance the brace count of the lines after it.
fn code_lines(text: &str) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut i = 0;
    while i < c.len() {
        let prev_ident = i > 0 && ident(c[i - 1]);
        if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
        } else if !prev_ident && (c[i] == 'r' || (c[i] == 'b' && c.get(i + 1) == Some(&'r'))) && {
            let mut j = i + if c[i] == 'b' { 2 } else { 1 };
            while c.get(j) == Some(&'#') {
                j += 1;
            }
            c.get(j) == Some(&'"')
        } {
            // A raw string: `r"…"`, `r#"…"#`, `br##"…"##`.
            let start = i;
            i += if c[i] == 'b' { 2 } else { 1 };
            let mut hashes = 0;
            while c[i] == '#' {
                hashes += 1;
                i += 1;
            }
            i += 1;
            while i < c.len() && !(c[i] == '"' && (1..=hashes).all(|k| c.get(i + k) == Some(&'#'))) {
                i += 1;
            }
            i = (i + 1 + hashes).min(c.len());
            c[start..i].iter().for_each(|&ch| blank(&mut out, ch));
        } else if c[i] == '"' {
            blank(&mut out, c[i]);
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' {
                    blank(&mut out, c[i]);
                    i += 1;
                }
                if i < c.len() {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
            if i < c.len() {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '\'' && (!prev_ident || (c[i - 1] == 'b' && (i < 2 || !ident(c[i - 2])))) {
            // A char or byte literal (`'x'`, `b'"'`, `'\u{..}'`); a lifetime (`'a`) has no
            // closing quote.
            let end = if c.get(i + 1) == Some(&'\\') {
                (i + 3..c.len().min(i + 12)).find(|&k| c[k] == '\'')
            } else if c.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match end {
                Some(end) => {
                    c[i..=end].iter().for_each(|&ch| blank(&mut out, ch));
                    i = end + 1;
                }
                None => {
                    out.push(c[i]);
                    i += 1;
                }
            }
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out.lines().map(str::to_string).collect()
}

/// Per line: is it product code (not a `#[cfg(test)]`/`#[test]` item)?
fn product_mask(code: &[String]) -> Vec<bool> {
    let mut keep = vec![true; code.len()];
    let mut i = 0;
    while i < code.len() {
        let t = code[i].trim_start();
        if t.starts_with("#[cfg(test)]") || t.starts_with("#[cfg(all(test") || t.starts_with("#[test]") {
            keep[i] = false;
            i += 1;
            while i < code.len() && code[i].trim_start().starts_with("#[") {
                keep[i] = false;
                i += 1;
            }
            // The item: to the brace that closes it, or the `;` (an item) or `,` (a field or an
            // argument) that ends a braceless one. A line that closes the enclosing scope first
            // is not the item's.
            let mut depth = 0i32;
            let mut opened = false;
            while i < code.len() {
                let (mut d, mut o) = (depth, opened);
                for c in code[i].chars() {
                    match c {
                        '{' => {
                            d += 1;
                            o = true;
                        }
                        '}' => d -= 1,
                        _ => {}
                    }
                }
                if !o && d < 0 {
                    break;
                }
                (depth, opened) = (d, o);
                keep[i] = false;
                let line = code[i].trim_end();
                i += 1;
                if (opened && depth <= 0) || (!opened && (line.ends_with(';') || line.ends_with(','))) {
                    break;
                }
            }
            continue;
        }
        i += 1;
    }
    keep
}

/// Line ranges (inclusive) of every `match` body whose literals name every harness variant.
fn exhaustive_ranges(code: &[String], keep: &[bool], literal: &Regex, all: &BTreeSet<String>) -> Vec<(usize, usize)> {
    let word = Regex::new(r"\bmatch\b").unwrap();
    let mut ranges = Vec::new();
    for (start, line) in code.iter().enumerate() {
        if !keep[start] {
            continue;
        }
        for m in word.find_iter(line) {
            // The body opens at the first `{` after `match`, on this line or a later one.
            let mut depth = 0i32;
            let mut opened = false;
            let mut end = None;
            let mut offset = m.end();
            let mut i = start;
            'scan: while i < code.len() {
                for c in code[i][offset..].chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => {
                            depth -= 1;
                            if opened && depth == 0 {
                                end = Some(i);
                                break 'scan;
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
                offset = 0;
            }
            let Some(end) = end else { continue };
            let named: BTreeSet<String> = (start..=end)
                .flat_map(|i| literal.captures_iter(&code[i]).map(|v| v[1].to_string()))
                .collect();
            if all.is_subset(&named) {
                ranges.push((start, end));
            }
        }
    }
    ranges
}

#[test]
fn harness_names_stay_inside_exhaustive_matches() {
    let roots = source_roots();
    let mut files = Vec::new();
    for (root, adapters) in &roots {
        let before = files.len();
        fenced_files(root, *adapters, &mut files);
        assert!(files.len() > before, "the fence read no files under {}", root.display());
    }
    assert!(files.len() > 20, "the fence read {} files: wrong root?", files.len());
    let alias = Regex::new(r"\bHarness\s+as\s+(\w+)|\btype\s+(\w+)\s*=\s*(?:\w+::)*Harness\s*;").unwrap();
    let variants_import = Regex::new(r"\bHarness::(?:\*|\{)").unwrap();
    let all: BTreeSet<String> = Harness::ALL.iter().map(|h| format!("{h:?}")).collect();

    // A file a parent declares under `#[cfg(test)] mod name;` is a test file by another name.
    let mod_decl = Regex::new(r"\bmod\s+(\w+)\s*;").unwrap();
    let mut test_only = BTreeSet::new();
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        let code = code_lines(&text);
        let keep = product_mask(&code);
        let dir = match path.file_name().and_then(|n| n.to_str()) {
            Some("mod.rs" | "lib.rs" | "main.rs") => path.parent().unwrap().to_path_buf(),
            _ => path.with_extension(""),
        };
        for (i, line) in code.iter().enumerate() {
            if let Some(m) = mod_decl.captures(line).filter(|_| !keep[i]) {
                test_only.insert(dir.join(format!("{}.rs", &m[1])));
                test_only.insert(dir.join(&m[1]).join("mod.rs"));
            }
        }
    }

    let mut sites = Vec::new();
    for path in files.iter().filter(|p| !test_only.contains(*p)) {
        let text = fs::read_to_string(path).unwrap();
        let code = code_lines(&text);
        let keep = product_mask(&code);
        let (root, _) = roots.iter().find(|(r, _)| path.starts_with(r)).unwrap();
        let crate_name = root.parent().unwrap().file_name().unwrap().to_str().unwrap();
        let rel = format!("{crate_name}/{}", path.strip_prefix(root).unwrap().to_str().unwrap());
        // `Harness` and every name this file's product code aliases it to.
        let mut names = vec!["Harness".to_string()];
        for (i, line) in code.iter().enumerate().filter(|(i, _)| keep[*i]) {
            for c in alias.captures_iter(line) {
                names.push(c.get(1).or(c.get(2)).unwrap().as_str().to_string());
            }
            if variants_import.is_match(line) {
                sites.push(format!("{rel}:{}: {}", i + 1, text.lines().nth(i).unwrap().trim()));
            }
        }
        let literal = Regex::new(&format!(r"\b(?:{})::([A-Z][A-Za-z]*)", names.join("|"))).unwrap();
        let fenced = exhaustive_ranges(&code, &keep, &literal, &all);
        for (i, line) in code.iter().enumerate() {
            if !keep[i] || fenced.iter().any(|(a, b)| (*a..=*b).contains(&i)) {
                continue;
            }
            for m in literal.captures_iter(line) {
                let name = &m[1];
                if name == "ALL" || name == "COUNT" {
                    continue;
                }
                sites.push(format!("{rel}:{}: {}", i + 1, text.lines().nth(i).unwrap().trim()));
            }
        }
    }

    assert!(
        sites.is_empty(),
        "adapter boundary fence breached:\n  {}\n\
         The law: a shared module names a harness only inside an exhaustive `match` (a per-harness \
         table). A special case for one harness is an `Adapter` method with a default. If the site \
         is a table, make the match name every variant.",
        sites.join("\n  ")
    );
}

#[test]
fn the_lexer_and_the_test_mask_hold_across_lines() {
    // A raw string spanning lines, a byte literal holding a quote, an escaped quote char and a
    // block comment leave no brace or quote behind; a lifetime is code.
    let text = "let a = r#\"{\n}\"# ; let b = b'\"'; let c = '\\''; /* { */ fn f<'a>() {}\n";
    let code = code_lines(text);
    assert_eq!(code.len(), 2);
    assert!(!code.iter().any(|l| l.contains('"')), "{code:?}");
    assert!(!code[0].contains('{'), "{code:?}");
    assert!(
        code[1].ends_with("fn f<'a>() {}") && code[1].matches('{').count() == 1,
        "{code:?}"
    );
    // A `#[cfg(test)]` field ends at its comma; the closing line stays product code.
    let lines: Vec<String> = ["Ok(S {", "    a,", "    #[cfg(test)]", "    b,", "})", "fn g() {}"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(product_mask(&lines), [true, true, false, false, true, true]);
}
