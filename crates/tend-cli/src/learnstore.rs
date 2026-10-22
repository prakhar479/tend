//! Shared learning on disk (F2): the machine's learned fixes and the projects' notes.
//!
//! <home>/learn/fixes.jsonl  one learned fix per line (any session, any project)
//! <home>/learn/notes.jsonl  the notes; a note from an agent waits for the user's review

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tend_core::learn::KnownFix;
use tend_core::{Body, Decision};

use crate::paths;

/// Fixes a session starts with, at most (the newest).
const MAX_KNOWN: usize = 2000;

fn dir() -> PathBuf {
    paths::home().join("learn")
}

/// The repo root of `cwd`, or `cwd` outside git: what "project" means here.
pub fn project_of(cwd: &Path) -> String {
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
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("fixes.jsonl"))
    {
        let _ = f.write_all(lines.as_bytes());
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

fn save_notes(notes: &[Note]) -> Result<()> {
    paths::ensure_private_dir(&dir())?;
    let text: String = notes
        .iter()
        .filter_map(|n| serde_json::to_string(n).ok())
        .map(|l| l + "\n")
        .collect();
    let tmp = dir().join("notes.jsonl.t");
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
    let by = std::env::var_os("TEND_SESSION_DIR")
        .and_then(|d| {
            PathBuf::from(d)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "user".into());
    let mut notes = load_notes();
    let id = format!("n-{}", notes.len() + 1);
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
    let mut notes = load_notes();
    if let Some((verb, id)) = action {
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
        return Ok(said);
    }
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
}
