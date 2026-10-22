//! Shared learning on disk (F2): the machine's learned fixes and the projects' notes.
//!
//! <home>/learn/fixes.jsonl  one learned fix per line (any session, any project)
//! <home>/learn/notes.jsonl  the notes; a note from an agent waits for the user's review

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tend_core::learn::KnownFix;
use tend_core::{Body, Decision};

use crate::paths;

/// Fixes a session starts with, at most (the newest). The store is cut back to this
/// many when it holds twice as many, so a session start stays quick.
const MAX_KNOWN: usize = 2000;

fn dir() -> PathBuf {
    paths::home().join("learn")
}

/// The repo root of `cwd`, or `cwd` outside git: what "project" means here.
pub fn project_of(cwd: &Path) -> String {
    // Every worktree of a repo is one project: a fleet worker's fixes and the user's
    // notes are shared with the main checkout. The main checkout is where the shared
    // `.git` is; without one (a submodule, a bare repo), the checkout's own root.
    let common = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    if let Some(main) = common
        .filter(|c| c.file_name().is_some_and(|n| n == ".git"))
        .and_then(|c| c.parent().map(Path::to_path_buf))
    {
        return main.display().to_string();
    }
    root_of(cwd)
}

/// The checkout `cwd` is in (its worktree): learned paths are relative to it.
pub fn root_of(cwd: &Path) -> String {
    crate::audit::repo_root(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

/// Other sessions' fixes, newest last.
pub fn load_fixes(except_session: &str) -> Vec<KnownFix> {
    load_fixes_in(&dir(), except_session)
}

fn load_fixes_in(dir: &Path, except_session: &str) -> Vec<KnownFix> {
    let Ok(text) = fs::read_to_string(dir.join("fixes.jsonl")) else {
        return Vec::new();
    };
    let mut fixes: Vec<KnownFix> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<KnownFix>(l).ok())
        .filter(|f| f.session != except_session)
        .collect();
    let skip = fixes.len().saturating_sub(MAX_KNOWN);
    fixes.drain(..skip);
    fixes
}

/// Append the fixes a decision learned. Fails open: the store is a convenience.
pub fn save_fixes(decisions: &[Decision], session: &str, project: &str) {
    save_fixes_in(&dir(), decisions, session, project)
}

fn save_fixes_in(dir: &Path, decisions: &[Decision], session: &str, project: &str) {
    let mut lines = String::new();
    for e in decisions.iter().flat_map(|d| &d.entries) {
        if let Body::Learned {
            signature,
            message,
            test,
            command,
            files,
        } = &e.body
        {
            let fix = KnownFix {
                signature: signature.clone(),
                message: message.clone(),
                test: test.clone(),
                command: command.clone(),
                files: files.clone(),
                session: session.to_string(),
                project: project.to_string(),
                at_ms: e.ts_ms,
            };
            if let Ok(l) = serde_json::to_string(&fix) {
                lines.push_str(&l);
                lines.push('\n');
            }
        }
    }
    if lines.is_empty() || paths::ensure_private_dir(dir).is_err() {
        return;
    }
    let path = dir.join("fixes.jsonl");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(lines.as_bytes());
    }
    // Keep it short: the newest MAX_KNOWN, once it holds twice that.
    if let Ok(text) = fs::read_to_string(&path) {
        let all: Vec<&str> = text.lines().collect();
        if all.len() > 2 * MAX_KNOWN {
            let keep = all[all.len() - MAX_KNOWN..].join("\n") + "\n";
            let tmp = dir.join(format!("fixes.jsonl.{}", std::process::id()));
            if fs::write(&tmp, keep).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub project: String,
    pub text: String,
    /// `user`, or the agent's session id.
    pub by: String,
    /// pending, approved, or dropped.
    pub state: String,
    pub ts_ms: u64,
}

fn load_notes() -> Vec<Note> {
    fs::read_to_string(dir().join("notes.jsonl"))
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Hold the notes lock (an flock on `notes.lock`) while `f` reads and writes them: two
/// sessions writing notes at once must not lose one.
fn with_notes_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::os::unix::io::AsRawFd;
    paths::ensure_private_dir(&dir())?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir().join("notes.lock"))?;
    // SAFETY: flock on an open file; the lock goes with the file when it is dropped.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    f()
}

fn save_notes(notes: &[Note]) -> Result<()> {
    paths::ensure_private_dir(&dir())?;
    let text: String = notes
        .iter()
        .filter_map(|n| serde_json::to_string(n).ok())
        .map(|l| l + "\n")
        .collect();
    let tmp = dir().join(format!("notes.jsonl.{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, dir().join("notes.jsonl"))?;
    Ok(())
}

/// A project's approved notes, oldest first.
pub fn approved_notes(project: &str) -> Vec<String> {
    load_notes()
        .into_iter()
        .filter(|n| n.project == project && n.state == "approved")
        .map(|n| n.text)
        .collect()
}

/// `tend note "…"`: from the user, kept; from an agent (in a tend session), pending.
pub fn note(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        bail!("a note needs text");
    }
    let project = project_of(&std::env::current_dir()?);
    // The user's own note: outside a tend session, typed in a terminal. An agent's
    // shell has no terminal, so clearing TEND_SESSION_DIR does not make it the user's.
    use std::io::IsTerminal;
    let by = match std::env::var_os("TEND_SESSION_DIR") {
        Some(d) => PathBuf::from(d)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "agent".into()),
        None if std::io::stdin().is_terminal() => "user".into(),
        None => "no terminal".into(),
    };
    with_notes_lock(|| add_note(project, text, by))
}

fn add_note(project: String, text: &str, by: String) -> Result<String> {
    let mut notes = load_notes();
    // One past the highest: ids are never reused, even after a rewrite.
    let next = notes
        .iter()
        .filter_map(|n| n.id.strip_prefix("n-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let id = format!("n-{next}");
    let state = if by == "user" { "approved" } else { "pending" };
    notes.push(Note {
        id: id.clone(),
        project,
        text: text.to_string(),
        by: by.clone(),
        state: state.into(),
        ts_ms: paths::now_ms(),
    });
    save_notes(&notes)?;
    Ok(if state == "approved" {
        format!("Kept {id}. New sessions in this project get it at start.")
    } else {
        format!("Saved {id} for the user's review (`tend notes`).")
    })
}

/// `tend notes [approve|drop <id>] [--all]`.
pub fn notes(action: Option<(&str, &str)>, all: bool) -> Result<String> {
    if let Some((verb, id)) = action {
        // Approving is the user's: an agent must not approve its own note.
        if std::env::var_os("TEND_SESSION_DIR").is_some() {
            bail!("approving notes is the user's; run it outside a tend session");
        }
        return with_notes_lock(|| {
            let mut notes = load_notes();
            let Some(n) = notes.iter_mut().find(|n| n.id == id) else {
                bail!("no note {id}");
            };
            n.state = if verb == "approve" {
                "approved"
            } else {
                "dropped"
            }
            .into();
            let said = format!("{id} is {}.", n.state);
            save_notes(&notes)?;
            Ok(said)
        });
    }
    let notes = load_notes();
    let project = project_of(&std::env::current_dir()?);
    let mut out = String::new();
    for n in notes
        .iter()
        .filter(|n| all || n.project == project)
        .filter(|n| n.state != "dropped")
    {
        let who = if n.by == "user" {
            "you".to_string()
        } else {
            format!("agent in {}", n.by)
        };
        let _ = writeln!(out, "{}  {:<8}  {}  ({who})", n.id, n.state, n.text);
    }
    if out.is_empty() {
        out.push_str("No notes for this project. `tend note \"…\"` keeps one.\n");
    } else if notes
        .iter()
        .any(|n| n.state == "pending" && (all || n.project == project))
    {
        out.push_str("Pending notes reach sessions once you `tend notes approve <id>`.\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_round_trip_and_skip_their_own_session() {
        let home = std::env::temp_dir().join(format!("tend-learn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let d = Decision {
            entries: vec![tend_core::Entry {
                seq: 3,
                ts_ms: 42,
                causes: vec![],
                body: Body::Learned {
                    signature: "s".into(),
                    message: "AssertionError".into(),
                    test: None,
                    command: "pytest".into(),
                    files: vec!["a.py".into()],
                },
            }],
            ..Decision::default()
        };
        save_fixes_in(&home, &[d], "sess-1", "/repo");
        assert!(load_fixes_in(&home, "sess-1").is_empty());
        let got = load_fixes_in(&home, "sess-2");
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].project.as_str(), got[0].at_ms), ("/repo", 42));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_fix_store_is_cut_back_when_it_doubles() {
        let home = std::env::temp_dir().join(format!("tend-learn-cut-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let dir = home.clone();
        fs::create_dir_all(&dir).unwrap();
        let old: String = (0..2 * MAX_KNOWN)
            .map(|i| format!("{{\"n\":{i}}}\n"))
            .collect();
        fs::write(dir.join("fixes.jsonl"), old).unwrap();
        let d = Decision {
            entries: vec![tend_core::Entry {
                seq: 1,
                ts_ms: 7,
                causes: vec![],
                body: Body::Learned {
                    signature: "s".into(),
                    message: "m".into(),
                    test: None,
                    command: "pytest".into(),
                    files: vec!["a.py".into()],
                },
            }],
            ..Decision::default()
        };
        save_fixes_in(&home, &[d], "sess-1", "/repo");
        let text = fs::read_to_string(dir.join("fixes.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_KNOWN);
        assert!(lines.last().unwrap().contains("sess-1"), "the newest stays");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_worktree_is_its_main_checkouts_project() {
        let d = std::env::temp_dir().join(format!("tend-learn-wt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let repo = d.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        let git = |at: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(at)
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&repo, &["worktree", "add", "-q", "../wt"]);
        let main = fs::canonicalize(&repo).unwrap().display().to_string();
        assert_eq!(project_of(&repo.join("src")), main);
        assert_eq!(project_of(&d.join("wt")), main);
        assert_eq!(
            root_of(&d.join("wt")),
            fs::canonicalize(d.join("wt"))
                .unwrap()
                .display()
                .to_string()
        );
        let plain = d.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_of(&plain), root_of(&plain));
        let _ = fs::remove_dir_all(&d);
    }
}
