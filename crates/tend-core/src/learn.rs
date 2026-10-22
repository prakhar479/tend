//! Shared learning (F2): fixes learned from this session, and fixes other sessions
//! learned, recalled when this session hits the same failure again.
//!
//! A fix: a command failed, files changed, and a later run of the same command no
//! longer showed that failure. Pure: the daemon loads other sessions' fixes and keeps
//! this session's in a machine-wide store.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::perception::Failure;

/// Files kept for one open failure, at most.
const MAX_FILES: usize = 20;
/// Recalls in one session, at most.
const MAX_RECALLS: usize = 10;

/// A fix another session learned, as the daemon loads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownFix {
    pub signature: String,
    pub message: String,
    #[serde(default)]
    pub test: Option<String>,
    pub command: String,
    pub files: Vec<String>,
    pub session: String,
    /// The repo root it was learned in.
    pub project: String,
    pub at_ms: u64,
}

/// A fix this session just learned.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixed {
    pub failure: Failure,
    pub command: String,
    pub files: Vec<String>,
}

#[derive(Debug, Default)]
struct Open {
    failures: Vec<Failure>,
    files: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct Learning {
    /// This session's project root: paths are kept relative to it.
    project: String,
    known: Vec<KnownFix>,
    /// Failing runs by command key, and the files changed since.
    open: BTreeMap<String, Open>,
    seen: BTreeMap<String, u32>,
    recalled: BTreeSet<String>,
}

impl Learning {
    pub fn set_context(&mut self, project: &str, known: Vec<KnownFix>) {
        self.project = project.trim_end_matches('/').to_string();
        self.known = known;
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    fn rel(&self, path: &str) -> String {
        match path.strip_prefix(&self.project) {
            Some(r) if !self.project.is_empty() => r.trim_start_matches('/').to_string(),
            _ => path.to_string(),
        }
    }

    /// A file changed: it may be part of the fix of every open failure.
    pub fn on_edit(&mut self, path: &str) {
        let rel = self.rel(path);
        for o in self.open.values_mut() {
            if o.files.len() < MAX_FILES {
                o.files.insert(rel.clone());
            }
        }
    }

    /// A test or build run of command `key`. `passed`: it is known to have exited 0.
    /// Failures of an earlier failing run that are gone now, with files changed in
    /// between, are learned.
    pub fn on_run(&mut self, key: &str, failures: &[Failure], passed: bool) -> Vec<Fixed> {
        let now: BTreeSet<&str> = failures.iter().map(|f| f.signature.as_str()).collect();
        let mut fixed = Vec::new();
        if let Some(o) = self.open.get_mut(key) {
            // Only what is gone counts, and only when the run says it is gone: a pass,
            // or a failing run whose failures the parsers read.
            if passed || !failures.is_empty() {
                let files: Vec<String> = o.files.iter().cloned().collect();
                if !files.is_empty() {
                    fixed = o
                        .failures
                        .iter()
                        .filter(|f| !now.contains(f.signature.as_str()) && !f.summary_only)
                        .map(|f| Fixed {
                            failure: f.clone(),
                            command: key.to_string(),
                            files: files.clone(),
                        })
                        .collect();
                }
            }
        }
        if passed && failures.is_empty() {
            self.open.remove(key);
        } else if !failures.is_empty() {
            let o = self.open.entry(key.to_string()).or_default();
            o.failures = failures.to_vec();
            if !fixed.is_empty() {
                o.files.clear();
            }
        }
        fixed
    }

    /// The second time this session hits a failure that another session fixed: that
    /// fix, the same project's first, then the newest. Once per failure.
    pub fn recall(&mut self, failures: &[Failure]) -> Option<(Failure, KnownFix)> {
        let mut out = None;
        for f in failures {
            let n = self.seen.entry(f.signature.clone()).or_insert(0);
            *n += 1;
            if out.is_some() || *n < 2 || self.recalled.contains(&f.signature) || f.summary_only {
                continue;
            }
            if self.recalled.len() >= MAX_RECALLS {
                continue;
            }
            let best = self
                .known
                .iter()
                .filter(|k| k.signature == f.signature)
                .max_by_key(|k| (k.project == self.project, k.at_ms));
            if let Some(k) = best {
                self.recalled.insert(f.signature.clone());
                out = Some((f.clone(), k.clone()));
            }
        }
        out
    }

    /// What the agent reads for a recalled fix.
    pub fn recall_text(&self, k: &KnownFix, now_ms: u64) -> String {
        let ago = now_ms.saturating_sub(k.at_ms) / 60_000;
        let when = match ago {
            0 => "minutes ago".to_string(),
            1..=119 => format!("{ago} min ago"),
            m if m < 48 * 60 => format!("{} h ago", m / 60),
            m => format!("{} days ago", m / (24 * 60)),
        };
        let place = if k.project == self.project {
            "in this project".to_string()
        } else {
            format!("in {}", k.project)
        };
        format!(
            "tend: this failure was fixed before, {place}, {when}: after changes to {}, `{}` passed. That may help here; check that it fits.",
            k.files.join(", "),
            k.command
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(sig: &str) -> Failure {
        Failure {
            signature: sig.into(),
            test: Some(format!("test_{sig}")),
            message: format!("AssertionError {sig}"),
            summary_only: false,
        }
    }

    #[test]
    fn a_failure_that_goes_away_after_edits_is_learned() {
        let mut l = Learning::default();
        l.set_context("/repo", vec![]);
        assert!(l
            .on_run("pytest", &[fail("a"), fail("b")], false)
            .is_empty());
        l.on_edit("/repo/src/x.py");
        // b is gone, a stays: b is learned.
        let f = l.on_run("pytest", &[fail("a")], false);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].failure.signature, "b");
        assert_eq!(f[0].files, ["src/x.py"]);
        l.on_edit("/repo/src/y.py");
        let f = l.on_run("pytest", &[], true);
        assert_eq!(
            f.iter()
                .map(|x| x.failure.signature.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(
            f[0].files,
            ["src/y.py"],
            "the files since the last learned fix"
        );
    }

    #[test]
    fn nothing_is_learned_without_edits_or_without_a_clear_result() {
        let mut l = Learning::default();
        l.on_run("pytest", &[fail("a")], false);
        assert!(
            l.on_run("pytest", &[], true).is_empty(),
            "a flaky pass with no edit"
        );
        l.on_run("pytest", &[fail("a")], false);
        l.on_edit("x.py");
        assert!(
            l.on_run("pytest", &[], false).is_empty(),
            "failed, but unreadable: not a fix"
        );
        assert!(
            l.on_run("cargo test", &[], true).is_empty(),
            "another command"
        );
    }

    #[test]
    fn the_second_hit_recalls_the_best_known_fix_once() {
        let fix = |project: &str, at: u64, files: &[&str]| KnownFix {
            signature: "a".into(),
            message: "AssertionError a".into(),
            test: None,
            command: "pytest".into(),
            files: files.iter().map(|s| s.to_string()).collect(),
            session: "s".into(),
            project: project.into(),
            at_ms: at,
        };
        let mut l = Learning::default();
        l.set_context(
            "/repo",
            vec![fix("/other", 900, &["o.py"]), fix("/repo", 100, &["r.py"])],
        );
        assert!(
            l.recall(&[fail("a")]).is_none(),
            "the first hit says nothing"
        );
        let (_, k) = l.recall(&[fail("a")]).expect("second hit");
        assert_eq!(k.files, ["r.py"], "this project's fix first");
        assert!(l.recall(&[fail("a")]).is_none(), "once");
        assert!(l
            .recall_text(&k, 100 + 3 * 60_000)
            .contains("in this project, 3 min ago: after changes to r.py, `pytest` passed"));
        assert!(Learning::default()
            .recall(&[fail("zz"), fail("zz")])
            .is_none());
    }
}
