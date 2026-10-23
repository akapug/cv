//! Embed the git commit into the binary (audit G11): `cv --version` prints `x.y.z (abc123)`,
//! so a running binary is checkable against source. `CV_BUILD_SHA` overrides (release pipelines);
//! outside a git checkout (crates.io builds) it degrades to "unknown".
//! The `-dirty` stamp requires watching every tracked source, including documentation: narrowing
//! the watch set would let an uncommitted edit retain a clean stamp. Output/cache dirs are excluded.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    Some(s)
}

fn watch_git_path(name: &str) {
    let path = git(&["rev-parse", "--path-format=absolute", "--git-path", name])
        .filter(|path| std::path::Path::new(path).is_absolute())
        .expect("git must support rev-parse --path-format=absolute to track build provenance");
    println!("cargo:rerun-if-changed={path}");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CV_BUILD_SHA");
    // Git resolves shared refs and per-worktree HEAD/index paths differently. In a
    // linked worktree, joining refs onto --absolute-git-dir watches the wrong file.
    if git(&["rev-parse", "--show-toplevel"]).is_some() {
        for name in ["HEAD", "packed-refs", "index"] {
            watch_git_path(name);
        }
        if let Some(reference) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
            watch_git_path(&reference);
        }
    }
    // An explicit rerun list otherwise suppresses rebuilds of the -dirty stamp
    // when tracked sources change without a commit. Do not watch output/cache dirs.
    if let (Some(root), Some(files)) = (
        git(&["rev-parse", "--show-toplevel"]),
        git(&["ls-files", "--full-name", "-z", ":/"]),
    ) {
        for file in files.split('\0').filter(|file| !file.is_empty()) {
            println!("cargo:rerun-if-changed={root}/{file}");
        }
    }
    let sha = std::env::var("CV_BUILD_SHA")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            git(&["rev-parse", "--short=12", "HEAD"]).map(|short| {
                match git(&["status", "--porcelain", "--untracked-files=no"]) {
                    Some(st) if !st.is_empty() => format!("{short}-dirty"),
                    Some(_) => short,
                    None => "unknown".to_string(),
                }
            })
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CV_BUILD_SHA={sha}");
}
