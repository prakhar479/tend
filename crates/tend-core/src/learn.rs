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
/// Edits kept for one open failure, and proposed edits waiting to land, at most.
const MAX_HUNKS: usize = 20;
/// Lines of one side of an edit kept before it is shown.
const MAX_HUNK_LINES: usize = 40;
/// A fix's change as shown: lines and characters, at most.
const MAX_CHANGE_LINES: usize = 30;
const MAX_CHANGE_CHARS: usize = 1500;
const MAX_CHANGE_LINE_CHARS: usize = 200;

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
    /// What the fix changed: a short, redacted diff (shown only in its project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
}

/// A run whose exit code can show a fix: the exit code is the test's (no pipe, no
/// `||`, nothing after it but `&&`), and no flag stops early or runs only some tests.
pub fn complete_run(command: &str) -> bool {
    !command.contains('|') && last_is_test(command) && !partial(command)
}

/// A run whose output can show a fix, by the runner's own summary: all of it ran, and
/// any pipe only keeps the end (`| tail -20`, `| tee log`), where the summary is.
pub fn summary_run(command: &str) -> bool {
    let keeps_end = command.split('|').skip(1).all(|stage| {
        let w = stage.split_whitespace().next().unwrap_or("");
        !stage.is_empty() && matches!(w, "tail" | "tee")
    });
    keeps_end && !command.contains("||") && last_is_test(command) && !partial(command)
}

/// `cat x; pytest` is fine; `pytest; echo done` exits with echo's code.
fn last_is_test(command: &str) -> bool {
    let last = command
        .split([';', '\n'])
        .map(str::trim)
        .rfind(|s| !s.is_empty())
        .unwrap_or("");
    crate::perception::classify::classify(last).kind == crate::perception::OutputKind::Test
}

/// A flag that stops early or runs only some tests.
fn partial(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.iter().enumerate().any(|(i, &w)| {
        // `python -m pytest` names a module; pytest's own `-m` picks tests.
        // `python -m pytest` and `python -mpytest` name a module; pytest's own `-m`
        // picks tests.
        let interpreter_flag = w.starts_with("-m") && i > 0 && words[i - 1].starts_with("python");
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
                || w.starts_with("--deselect")
                || (w.starts_with("-k") || w.starts_with("-m"))
                    && !w.starts_with("--")
                    && w.len() > 2
                    && !interpreter_flag
                || w.starts_with("--bail=")
                || w.starts_with("-run="))
    })
}

/// A fix this session just learned.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixed {
    pub failure: Failure,
    pub command: String,
    pub files: Vec<String>,
    pub change: Option<String>,
}

/// One edit's changed lines (the lines both sides share are left out).
#[derive(Debug, Clone, PartialEq)]
struct Hunk {
    path: String,
    old: Vec<String>,
    new: Vec<String>,
}

impl Hunk {
    fn new(path: String, old: &str, new: &str) -> Self {
        let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
        let pre = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
        let suf = o[pre..]
            .iter()
            .rev()
            .zip(n[pre..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let keep = |v: &[&str]| {
            v.iter()
                .take(MAX_HUNK_LINES)
                .map(|l| l.to_string())
                .collect()
        };
        Hunk {
            path,
            old: keep(&o[pre..o.len() - suf]),
            new: keep(&n[pre..n.len() - suf]),
        }
    }
}

/// A fix's edits as a short unified-style text: `--- path`, then `-` and `+` lines.
/// Redacted and capped; None when no edit gave text (shell writes).
fn render_change(hunks: &[Hunk]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut last = "";
    for h in hunks
        .iter()
        .filter(|h| !(h.old.is_empty() && h.new.is_empty()))
    {
        if h.path != last {
            lines.push(format!("--- {}", h.path));
            last = &h.path;
        }
        lines.extend(h.old.iter().map(|l| format!("-{l}")));
        lines.extend(h.new.iter().map(|l| format!("+{l}")));
    }
    if lines.is_empty() {
        return None;
    }
    // Redact before capping, so a cut cannot split a secret; then cap each line.
    let lines: Vec<String> = crate::text::redact(&lines.join("\n"))
        .lines()
        .map(|l| crate::text::truncate_chars(l, MAX_CHANGE_LINE_CHARS))
        .collect();
    let total = lines.len();
    let mut out = String::new();
    let mut shown = 0;
    for l in &lines {
        if shown >= MAX_CHANGE_LINES || out.len() + l.len() > MAX_CHANGE_CHARS {
            break;
        }
        out.push_str(l);
        out.push('\n');
        shown += 1;
    }
    if shown < total {
        out.push_str(&format!("… ({} more lines)\n", total - shown));
    }
    Some(out.trim_end().to_string())
}

#[derive(Debug, Default)]
struct Open {
    failures: Vec<Failure>,
    files: BTreeSet<String>,
    hunks: Vec<Hunk>,
}

#[derive(Debug, Default)]
pub struct Learning {
    /// The project, as fixes and notes are kept: one name for all its worktrees.
    project: String,
    /// This session's checkout (its worktree): paths are kept relative to it.
    root: String,
    known: Vec<KnownFix>,
    /// Failing runs by command key, and the files changed since.
    open: BTreeMap<String, Open>,
    /// Edits proposed (PreToolUse) that have not landed yet, by tool use id.
    proposed: BTreeMap<String, Hunk>,
    seen: BTreeMap<String, u32>,
    recalled: BTreeSet<String>,
}

impl Learning {
    pub fn set_context(&mut self, project: &str, root: &str, known: Vec<KnownFix>) {
        self.project = project.trim_end_matches('/').to_string();
        self.root = root.trim_end_matches('/').to_string();
        self.known = known;
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    /// Fixes other sessions learned since this one started (a fleet's workers learn
    /// while their siblings run). Ones already known are skipped.
    pub fn add_known(&mut self, fixes: Vec<KnownFix>) {
        let mut have: BTreeSet<(String, String, u64)> = self
            .known
            .iter()
            .map(|k| (k.session.clone(), k.signature.clone(), k.at_ms))
            .collect();
        for f in fixes {
            if have.insert((f.session.clone(), f.signature.clone(), f.at_ms)) {
                self.known.push(f);
            }
        }
    }

    /// An edit the agent proposes: its text joins a fix only once it lands.
    pub fn on_proposed(&mut self, tool_use_id: &str, path: &str, old: &str, new: &str) {
        if self.open.is_empty() {
            return;
        }
        if self.proposed.len() >= MAX_HUNKS {
            self.proposed.pop_first();
        }
        let h = Hunk::new(self.rel(path), old, new);
        self.proposed.insert(tool_use_id.to_string(), h);
    }

    /// An edit landed (`FileEdited`): the file, and the text it was proposed with.
    pub fn on_edited(&mut self, tool_use_id: &str, path: &str) {
        let hunk = self.proposed.remove(tool_use_id);
        self.add_edit(path, hunk);
    }

    /// A shell command changed a file the connector snapshotted (`FileChanged`).
    pub fn on_changed(&mut self, path: &str, old: &str, new: &str) {
        if self.open.is_empty() {
            return;
        }
        let h = Hunk::new(self.rel(path), old, new);
        self.add_edit(path, Some(h));
    }

    fn rel(&self, path: &str) -> String {
        match path.strip_prefix(&self.root) {
            // `/repo2/x` is not inside `/repo`.
            Some(r) if !self.root.is_empty() && (r.is_empty() || r.starts_with('/')) => {
                r.trim_start_matches('/').to_string()
            }
            _ => path.to_string(),
        }
    }

    /// A file changed (with its text, when known): part of the fix of every open
    /// failure. A shell write with no snapshot names only the file.
    pub fn on_edit(&mut self, path: &str) {
        self.add_edit(path, None);
    }

    fn add_edit(&mut self, path: &str, hunk: Option<Hunk>) {
        let rel = self.rel(path);
        for o in self.open.values_mut() {
            if o.files.len() < MAX_FILES {
                o.files.insert(rel.clone());
            }
            if let Some(h) = &hunk {
                if o.hunks.len() < MAX_HUNKS {
                    o.hunks.push(h.clone());
                }
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
            let change = render_change(&o.hunks);
            return o
                .failures
                .into_iter()
                .filter(|f| !f.summary_only)
                .map(|failure| Fixed {
                    failure,
                    command: key.to_string(),
                    files: files.clone(),
                    change: change.clone(),
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

    /// A failure another session fixed: that fix, the same project's first, then the
    /// newest. On the first hit when this project's fix has its change (specific, and
    /// cheap to check); otherwise on the second hit. Once per failure.
    pub fn recall(&mut self, failures: &[Failure]) -> Option<(Failure, KnownFix)> {
        let mut out = None;
        for f in failures {
            let n = self.seen.entry(f.signature.clone()).or_insert(0);
            *n += 1;
            let n = *n;
            if out.is_some() || self.recalled.contains(&f.signature) || f.summary_only {
                continue;
            }
            if self.recalled.len() >= MAX_RECALLS {
                continue;
            }
            let best = self
                .known
                .iter()
                .filter(|k| k.signature == f.signature)
                .max_by_key(|k| (k.project == self.project, k.change.is_some(), k.at_ms))
                .filter(|k| n >= 2 || (k.project == self.project && k.change.is_some()));
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
        let files = k.files.join(", ");
        match (&k.change, k.project == self.project) {
            // The change stays in its project (owner, 2026-10-22).
            (Some(change), true) => format!(
                "tend: this failure was fixed before, in this project, {when}: another session changed {files}, then `{}` passed. The change:\n{change}\nIt is another session's change: check that it fits here before you use it.",
                k.command
            ),
            (_, here) => {
                let place = if here {
                    "in this project".to_string()
                } else {
                    format!("in {}", k.project)
                };
                format!(
                    "tend: this failure was fixed before, {place}, {when}: after changes to {files}, `{}` passed. That may help here; check that it fits.",
                    k.command
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_selectors_are_partial_runs() {
        assert!(!complete_run("pytest -kfoo"));
        assert!(!complete_run("pytest -mslow"));
        assert!(!complete_run("pytest --deselect tests/a.py::t"));
        assert!(complete_run("pytest tests"));
    }

    #[test]
    fn a_long_changed_line_still_shows_its_change() {
        let long = "x".repeat(5000);
        let h = Hunk::new("a.py".into(), "a\n", &format!("a\n{long}\n"));
        let c = render_change(&[h]).unwrap();
        assert!(c.contains("+xxx"), "{c}");
        assert!(c.len() < MAX_CHANGE_CHARS + 100);
    }

    #[test]
    fn rel_ignores_sibling_dir_with_shared_prefix() {
        let mut l = Learning::default();
        l.set_context("/p", "/work/repo", vec![]);
        assert_eq!(l.rel("/work/repo/src/a.py"), "src/a.py");
        assert_eq!(l.rel("/work/repo2/a.py"), "/work/repo2/a.py");
    }

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
        l.set_context("/repo", "/repo", vec![]);
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
            change: None,
        };
        let mut l = Learning::default();
        l.set_context(
            "/repo",
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
            "npm test || true",
            "pytest; echo done",
            "python3 run_tests.py; echo \"exit=$?\"",
        ] {
            assert!(!complete_run(cmd), "{cmd}");
        }
        for cmd in [
            "pytest",
            "cargo test",
            "pytest tests/ -q",
            "make check",
            "python3 -m pytest -v",
            "cat x.py; python3 run_tests.py parse",
            "cd a && pytest && echo ok",
        ] {
            assert!(complete_run(cmd), "{cmd}");
        }
        // Piped through tail or tee: the summary in the output decides.
        for cmd in [
            "python3 run_tests.py parse 2>&1 | tail -5",
            "pytest -q | tee /tmp/log",
        ] {
            assert!(summary_run(cmd) && !complete_run(cmd), "{cmd}");
        }
        for cmd in [
            "pytest | head -20",
            "pytest | grep FAIL",
            "pytest -x | tail",
            "npm test || true",
        ] {
            assert!(!summary_run(cmd), "{cmd}");
        }
    }

    #[test]
    fn a_worktree_learns_paths_from_its_own_checkout() {
        // A fleet worker: the project is the main checkout; its files are in a worktree.
        let mut l = Learning::default();
        l.set_context("/repo", "/fleets/f/worktrees/worker-1", vec![]);
        l.on_run("pytest", &[fail("a")], false);
        l.on_edit("/fleets/f/worktrees/worker-1/src/x.py");
        let f = l.on_run("pytest", &[], true);
        assert_eq!(f[0].files, ["src/x.py"]);
        assert_eq!(l.project(), "/repo");
    }

    #[test]
    fn a_fix_keeps_the_edits_that_landed() {
        let mut l = Learning::default();
        l.set_context("/repo", "/wt", vec![]);
        l.on_run("pytest", &[fail("a")], false);
        l.on_proposed(
            "e1",
            "/wt/kv/fmt.py",
            "def f(x):\n    return '%.2f' % x\n",
            "def f(x):\n    return '{:,.2f}'.format(x)\n",
        );
        l.on_edited("e1", "/wt/kv/fmt.py");
        // Proposed and denied (no FileEdited): not part of the fix.
        l.on_proposed("e2", "/wt/tests/test_fmt.py", "assert x", "assert True");
        // A shell write the connector snapshotted: only the changed lines.
        l.on_changed("/wt/kv/a.py", "one\ntwo\nthree\n", "one\nTWO\nthree\n");
        let f = l.on_run("pytest", &[], true);
        assert_eq!(f[0].files, ["kv/a.py", "kv/fmt.py"]);
        assert_eq!(
            f[0].change.as_deref(),
            Some("--- kv/fmt.py\n-    return '%.2f' % x\n+    return '{:,.2f}'.format(x)\n--- kv/a.py\n-two\n+TWO")
        );
    }

    #[test]
    fn a_shell_write_without_a_snapshot_names_the_file_only() {
        let mut l = Learning::default();
        l.set_context("/repo", "/repo", vec![]);
        l.on_run("pytest", &[fail("a")], false);
        l.on_edit("/repo/kv/x.py");
        let f = l.on_run("pytest", &[], true);
        assert_eq!(
            (f[0].files.clone(), f[0].change.clone()),
            (vec!["kv/x.py".to_string()], None)
        );
    }

    #[test]
    fn a_change_is_capped_and_redacted() {
        let mut l = Learning::default();
        l.set_context("/repo", "/repo", vec![]);
        l.on_run("pytest", &[fail("a")], false);
        let new: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let new = format!("TOKEN = \"ghp_{}\"\n{new}", "a".repeat(36));
        l.on_proposed("e1", "/repo/big.py", "", &new);
        l.on_edited("e1", "/repo/big.py");
        let change = l.on_run("pytest", &[], true)[0].change.clone().unwrap();
        assert!(!change.contains("ghp_aaaa"), "{change}");
        assert!(change.ends_with("more lines)"), "{change}");
        assert!(change.lines().count() <= MAX_CHANGE_LINES + 1);
    }

    #[test]
    fn a_same_project_fix_with_its_change_is_recalled_on_the_first_hit() {
        let fix = |project: &str, change: Option<&str>| KnownFix {
            signature: "a".into(),
            message: "AssertionError a".into(),
            test: None,
            command: "pytest".into(),
            files: vec!["kv/fmt.py".into()],
            session: "s".into(),
            project: project.into(),
            at_ms: 0,
            change: change.map(String::from),
        };
        // This project, with the change: the first hit, and the change is shown.
        let mut l = Learning::default();
        l.set_context(
            "/repo",
            "/repo",
            vec![fix("/repo", Some("--- kv/fmt.py\n-a\n+b"))],
        );
        let (_, k) = l.recall(&[fail("a")]).expect("first hit");
        let text = l.recall_text(&k, 0);
        assert!(
            text.contains("The change:\n--- kv/fmt.py\n-a\n+b"),
            "{text}"
        );
        // Another project's fix, even with a change: the second hit, files only.
        let mut l = Learning::default();
        l.set_context(
            "/repo",
            "/repo",
            vec![fix("/other", Some("--- kv/fmt.py\n-a\n+b"))],
        );
        assert!(l.recall(&[fail("a")]).is_none());
        let (_, k) = l.recall(&[fail("a")]).expect("second hit");
        let text = l.recall_text(&k, 0);
        assert!(!text.contains("+b") && text.contains("in /other"), "{text}");
        // This project without a change: the second hit.
        let mut l = Learning::default();
        l.set_context("/repo", "/repo", vec![fix("/repo", None)]);
        assert!(l.recall(&[fail("a")]).is_none());
        assert!(l.recall(&[fail("a")]).is_some());
    }

    #[test]
    fn fixes_learned_later_are_added_once() {
        let mut l = Learning::default();
        l.set_context("/repo", "/repo", vec![]);
        let k = KnownFix {
            signature: "a".into(),
            message: String::new(),
            test: None,
            command: "pytest".into(),
            files: vec![],
            session: "s".into(),
            project: "/repo".into(),
            at_ms: 1,
            change: Some("x".into()),
        };
        l.add_known(vec![k.clone()]);
        l.add_known(vec![k]);
        assert_eq!(l.known.len(), 1);
        assert!(l.recall(&[fail("a")]).is_some());
    }

    #[test]
    fn an_old_store_line_without_a_change_still_loads() {
        let line = r#"{"signature":"a","message":"m","command":"pytest","files":["x.py"],"session":"s","project":"/repo","at_ms":1}"#;
        let k: KnownFix = serde_json::from_str(line).unwrap();
        assert_eq!(k.change, None);
    }
}
