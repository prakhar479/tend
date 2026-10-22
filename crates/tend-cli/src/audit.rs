//! `tend audit` (experimental): what a git diff, or a tend session, changed in the
//! project's tests and checks. Read-only on the repository: the working tree is read
//! through a temporary index, never the user's.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tend_core::audit::{audit, FileChange, Report};

/// Files larger than this are listed by path only (lockfiles, data).
const MAX_TEXT_BYTES: u64 = 2 << 20;

pub const LABEL: &str = "tend audit (experimental)";

fn git(root: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let out = cmd.output().context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The top of the git work tree that holds `dir`.
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git(dir, &["rev-parse", "--show-toplevel"], None)?.trim(),
    ))
}

/// A tree object for the working tree as it is now, tracked and untracked files
/// (ignored ones left out). Uses a copy of the index, so the user's index, branch, and
/// files are untouched; only new blob objects are written.
pub fn worktree_tree(root: &Path, scratch: &Path) -> Result<String> {
    // One index per call: the hub snapshots worktrees from more than one thread.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = scratch.join(format!("audit-index-{}-{n}", std::process::id()));
    let real = PathBuf::from(git(root, &["rev-parse", "--git-path", "index"], None)?.trim());
    let real = if real.is_absolute() {
        real
    } else {
        root.join(real)
    };
    if real.exists() {
        std::fs::copy(&real, &index).context("copy the git index")?;
        // Keep the index's time: git re-reads files changed in the same second as the
        // index was written only while the index is not newer than them. A copy made
        // now would hide such a change (same size, same second).
        if let Ok(m) = std::fs::metadata(&real).and_then(|m| m.modified()) {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&index) {
                let _ = f.set_modified(m);
            }
        }
    }
    let tree = git(root, &["add", "-A"], Some(&index))
        .and_then(|_| git(root, &["write-tree"], Some(&index)));
    let _ = std::fs::remove_file(&index);
    Ok(tree?.trim().to_string())
}

/// The changes between two tree-ishes (commits or trees).
pub fn changes(root: &Path, from: &str, to: &str) -> Result<Vec<FileChange>> {
    let list = git(
        root,
        &["diff", "--name-status", "-z", "--no-renames", from, to],
        None,
    )?;
    let mut fields = list.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let read = |rev: &str| -> Option<String> {
            let spec = format!("{rev}:{path}");
            let size: u64 = git(root, &["cat-file", "-s", &spec], None)
                .ok()?
                .trim()
                .parse()
                .ok()?;
            if size > MAX_TEXT_BYTES {
                return Some(String::new());
            }
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["cat-file", "blob", &spec])
                .output()
                .ok()?;
            // Binary files are listed by path only.
            Some(String::from_utf8(out.stdout).unwrap_or_default())
        };
        let (old, new) = match status.chars().next() {
            Some('A') => (None, read(to)),
            Some('D') => (read(from), None),
            _ => (read(from), read(to)),
        };
        out.push(FileChange {
            path: path.to_string(),
            old,
            new,
        });
    }
    Ok(out)
}

/// `A..B`, `A...B` (from their merge base), `A` (to the working tree), or nothing
/// (HEAD to the working tree). Returns the two sides as tree-ishes.
pub fn resolve_range(root: &Path, range: Option<&str>, scratch: &Path) -> Result<(String, String)> {
    let worktree = || worktree_tree(root, scratch);
    match range {
        None => Ok(("HEAD".into(), worktree()?)),
        Some(r) => {
            if let Some((a, b)) = r.split_once("...") {
                let base = git(root, &["merge-base", a, b], None)?.trim().to_string();
                Ok((base, b.to_string()))
            } else if let Some((a, b)) = r.split_once("..") {
                Ok((a.to_string(), b.to_string()))
            } else {
                Ok((r.to_string(), worktree()?))
            }
        }
    }
}

pub fn run(root: &Path, from: &str, to: &str, ignore: &dyn Fn(&str) -> bool) -> Result<Report> {
    Ok(audit(&changes(root, from, to)?, ignore))
}

pub fn render_text(r: &Report, title: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{LABEL}: {title}");
    let _ = writeln!(out, "{}", r.one_line());
    for t in &r.tests {
        let mark = if t.weakened.is_empty() { " " } else { "!" };
        let _ = writeln!(
            out,
            "{mark} {} ({}): assertions {} → {}, tests {} → {}{}",
            t.path,
            t.status,
            t.assertions.0,
            t.assertions.1,
            t.tests.0,
            t.tests.1,
            if t.weakened.is_empty() {
                String::new()
            } else {
                format!(" · {}", t.weakened.join("; "))
            }
        );
    }
    for s in &r.snapshots {
        let _ = writeln!(out, "  snapshot rewritten: {s}");
    }
    for s in &r.suppressions {
        let _ = writeln!(
            out,
            "  {} {}:{}{}: {}",
            s.kind,
            s.path,
            s.line,
            if s.has_reason {
                ""
            } else {
                " (no reason given)"
            },
            s.text
        );
    }
    for c in &r.config {
        let _ = writeln!(
            out,
            "{} check config {}{}",
            if c.weaker.is_empty() { " " } else { "!" },
            c.path,
            if c.weaker.is_empty() {
                String::new()
            } else {
                format!(": {}", c.weaker.join("; "))
            }
        );
    }
    for d in &r.dependencies {
        let _ = writeln!(out, "  dependencies: {d}");
    }
    out
}

pub fn render_markdown(r: &Report, title: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### {LABEL}: {title}\n");
    let _ = writeln!(out, "{}\n", r.one_line());
    if !r.tests.is_empty() {
        let _ = writeln!(
            out,
            "| Test file | Change | Assertions | Tests | May weaken |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|");
        for t in &r.tests {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} → {} | {} → {} | {} |",
                t.path,
                t.status,
                t.assertions.0,
                t.assertions.1,
                t.tests.0,
                t.tests.1,
                t.weakened.join("; ")
            );
        }
        let _ = writeln!(out);
    }
    let mut list = |title: &str, items: Vec<String>| {
        if !items.is_empty() {
            let _ = writeln!(out, "**{title}**\n");
            for i in items {
                let _ = writeln!(out, "- {i}");
            }
            let _ = writeln!(out);
        }
    };
    list(
        "Snapshots rewritten",
        r.snapshots.iter().map(|s| format!("`{s}`")).collect(),
    );
    list(
        "Suppressions added",
        r.suppressions
            .iter()
            .map(|s| {
                format!(
                    "`{}:{}` {}{}",
                    s.path,
                    s.line,
                    s.kind,
                    if s.has_reason {
                        ""
                    } else {
                        " (no reason given)"
                    }
                )
            })
            .collect(),
    );
    list(
        "Check config",
        r.config
            .iter()
            .map(|c| {
                if c.weaker.is_empty() {
                    format!("`{}` changed", c.path)
                } else {
                    format!("`{}`: {}", c.path, c.weaker.join("; "))
                }
            })
            .collect(),
    );
    list(
        "Dependencies",
        r.dependencies.iter().map(|d| format!("`{d}`")).collect(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A change of the same size in the same second as the commit must show in the
    /// snapshot (the copied index once hid it, M1).
    #[test]
    fn a_same_size_change_right_after_a_commit_is_seen() {
        let d = std::env::temp_dir().join(format!("tend-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&d)
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false"
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        std::fs::write(d.join("f.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let head = git_out_for_test(&d);
        std::fs::write(d.join("f.txt"), "A\n").unwrap();
        let scratch = d.join(".scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let tree = worktree_tree(&d, &scratch).unwrap();
        assert_ne!(tree, head, "the change to f.txt is in the snapshot");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn git_out_for_test(d: &Path) -> String {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
}
