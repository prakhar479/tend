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

/// A run that can show a fix: no pipe (it hides the exit code and can cut the summary),
/// and no flag that stops early or runs only some tests.
pub fn complete_run(command: &str) -> bool {
    let piped = command.replace("||", "").contains('|');
    let words: Vec<&str> = command.split_whitespace().collect();
    let partial = words.iter().enumerate().any(|(i, &w)| {
        // `python -m pytest` names a module; pytest's own `-m` picks tests.
        let interpreter_flag = w == "-m" && i > 0 && words[i - 1].starts_with("python");
        !interpreter_flag
            && (matches!(
                w,
                "-x" | "--exitfirst"
                    | "--lf"
                    | "--last-failed"
                    | "-k"
                    | "-m"
                    | "--bail"
                    | "-failfast"
                    | "--fail-fast"
                    | "-run"
            ) || w.starts_with("--maxfail")
                || w.starts_with("--bail=")
                || w.starts_with("-run="))
    });
    !piped && !partial
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

    /// A test or build run of command `key`. `passed`: it exited 0, showed no failure,
    /// and ran everything (`complete_run`). Only such a pass, after files changed,
    /// teaches fixes: a failing run whose failures differ may have stopped early or
    /// broken elsewhere (a syntax error hides the test that failed before).
    pub fn on_run(&mut self, key: &str, failures: &[Failure], passed: bool) -> Vec<Fixed> {
        if passed && failures.is_empty() {
            let Some(o) = self.open.remove(key) else {
                return Vec::new();
            };
            let files: Vec<String> = o.files.into_iter().collect();
            if files.is_empty() {
                return Vec::new();
            }
            return o
                .failures
                .into_iter()
                .filter(|f| !f.summary_only)
                .map(|failure| Fixed {
                    failure,
                    command: key.to_string(),
                    files: files.clone(),
                })
                .collect();
        }
        if !failures.is_empty() {
            // Every failure seen since the first failing run: a pass fixes them all.
            let o = self.open.entry(key.to_string()).or_default();
            for f in failures {
                if !o.failures.iter().any(|x| x.signature == f.signature) {
                    o.failures.push(f.clone());
                }
            }
        }
        Vec::new()
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
    fn failures_that_a_later_pass_shows_gone_are_learned() {
        let mut l = Learning::default();
        l.set_context("/repo", vec![]);
        assert!(l
            .on_run("pytest", &[fail("a"), fail("b")], false)
            .is_empty());
        l.on_edit("/repo/src/x.py");
        // b is gone, a stays: a failing run teaches nothing (it may have stopped early).
        assert!(l.on_run("pytest", &[fail("a")], false).is_empty());
        l.on_edit("/repo/src/y.py");
        let f = l.on_run("pytest", &[], true);
        let sigs: Vec<&str> = f.iter().map(|x| x.failure.signature.as_str()).collect();
        assert_eq!(sigs, ["a", "b"], "the pass shows both gone");
        assert_eq!(f[0].files, ["src/x.py", "src/y.py"]);
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

    #[test]
    fn only_complete_runs_can_show_a_fix() {
        for cmd in [
            "pytest -x",
            "pytest | tail -20",
            "pytest --lf",
            "pytest -k api",
            "go test -run TestX ./...",
            "pytest --maxfail=1",
            "python -m pytest -m slow",
        ] {
            assert!(!complete_run(cmd), "{cmd}");
        }
        for cmd in [
            "pytest",
            "cargo test",
            "pytest tests/ -q",
            "npm test || true",
            "make check",
        ] {
            assert!(complete_run(cmd), "{cmd}");
        }
    }
}
