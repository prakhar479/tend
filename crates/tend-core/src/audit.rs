//! Audit (experimental): what a set of file changes did to the project's checks.
//!
//! Input is the old and new text of each changed file, from any source (a git diff,
//! a session's start and end). Output is a report a reviewer can read in a minute:
//! - test files: assertions and tests added and removed, and the guard's weakening rules;
//! - snapshot and golden files rewritten;
//! - suppressions added (`# noqa`, `# type: ignore`, `eslint-disable`, `#[allow]`, …),
//!   with or without a stated reason;
//! - check configuration changed (CI workflows, coverage thresholds, lint and type
//!   settings), with thresholds that went down;
//! - dependency manifests changed.
//!
//! The audit is a feature under test (owner decision, 2026-10-20): it reports, it never
//! blocks.

use serde::{Deserialize, Serialize};

use crate::broker::Answer;
use crate::guard;

/// One changed file. `None` means the file does not exist on that side.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub tests: Vec<TestChange>,
    pub snapshots: Vec<String>,
    pub suppressions: Vec<Suppression>,
    pub config: Vec<ConfigChange>,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestChange {
    pub path: String,
    /// added, deleted, or modified
    pub status: String,
    pub assertions: (usize, usize),
    pub tests: (usize, usize),
    /// The guard's reasons, when the change may weaken tests.
    pub weakened: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Suppression {
    pub path: String,
    /// 1-based line in the new text.
    pub line: usize,
    pub kind: String,
    pub text: String,
    /// The suppression says why (`# noqa: BLE001 - one failed unit must not kill a run`).
    pub has_reason: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigChange {
    pub path: String,
    /// What looks weaker, if anything: "coverage threshold 90 → 80", "strict mode off".
    pub weaker: Vec<String>,
}

impl Report {
    /// Files that changed in a way a reviewer should look at first.
    pub fn flagged(&self) -> usize {
        self.tests.iter().filter(|t| !t.weakened.is_empty()).count()
            + self.config.iter().filter(|c| !c.weaker.is_empty()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.tests.is_empty()
            && self.snapshots.is_empty()
            && self.suppressions.is_empty()
            && self.config.is_empty()
            && self.dependencies.is_empty()
    }

    /// One line: "4 test files changed (1 may weaken tests) · 6 suppressions added · …".
    pub fn one_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.tests.is_empty() {
            let weak = self.tests.iter().filter(|t| !t.weakened.is_empty()).count();
            let removed: usize = self
                .tests
                .iter()
                .map(|t| t.tests.0.saturating_sub(t.tests.1))
                .sum();
            let mut s = plural(self.tests.len(), "test file changed", "test files changed");
            if removed > 0 {
                s.push_str(&format!(
                    ", {}",
                    plural(removed, "test removed", "tests removed")
                ));
            }
            if weak > 0 {
                s.push_str(&format!(" ({weak} may weaken tests)"));
            }
            parts.push(s);
        }
        if !self.snapshots.is_empty() {
            parts.push(plural(
                self.snapshots.len(),
                "snapshot rewritten",
                "snapshots rewritten",
            ));
        }
        if !self.suppressions.is_empty() {
            parts.push(plural(
                self.suppressions.len(),
                "suppression added",
                "suppressions added",
            ));
        }
        let weaker = self.config.iter().filter(|c| !c.weaker.is_empty()).count();
        if weaker > 0 {
            parts.push(plural(
                weaker,
                "check config weakened",
                "check configs weakened",
            ));
        } else if !self.config.is_empty() {
            parts.push(plural(
                self.config.len(),
                "check config changed",
                "check configs changed",
            ));
        }
        if !self.dependencies.is_empty() {
            parts.push(plural(
                self.dependencies.len(),
                "dependency file changed",
                "dependency files changed",
            ));
        }
        if parts.is_empty() {
            "no changes to tests or checks".into()
        } else {
            parts.join(" · ")
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Audit a set of changes. `ignore` says which paths to leave out (vendored code).
pub fn audit(changes: &[FileChange], ignore: &dyn Fn(&str) -> bool) -> Report {
    let mut r = Report::default();
    for c in changes {
        if ignore(&c.path) {
            continue;
        }
        let (old, new) = (
            c.old.as_deref().unwrap_or(""),
            c.new.as_deref().unwrap_or(""),
        );
        let status = match (&c.old, &c.new) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            _ => "modified",
        };
        if is_snapshot(&c.path) {
            r.snapshots.push(c.path.clone());
            continue;
        }
        if guard::may_hold_tests(&c.path) && guard::judges(&c.path, old, new) {
            let (a_old, t_old) = guard::counts(old);
            let (a_new, t_new) = guard::counts(new);
            let weakened = match guard::assess(old, new) {
                Answer::Weakened { reasons } => reasons,
                _ => Vec::new(),
            };
            if (a_old, t_old) != (a_new, t_new) || !weakened.is_empty() || status != "modified" {
                r.tests.push(TestChange {
                    path: c.path.clone(),
                    status: status.into(),
                    assertions: (a_old, a_new),
                    tests: (t_old, t_new),
                    weakened,
                });
            }
        }
        r.suppressions.extend(suppressions_added(&c.path, old, new));
        if is_check_config(&c.path) {
            r.config.push(ConfigChange {
                path: c.path.clone(),
                weaker: config_weaker(old, new),
            });
        }
        if is_dependency_manifest(&c.path) {
            r.dependencies.push(c.path.clone());
        }
    }
    r
}

/// Snapshot and golden files: expected outputs a tool can rewrite wholesale.
pub fn is_snapshot(path: &str) -> bool {
    re!(
        r"(^|/)__snapshots__/|\.snap$|\.snap\.new$|\.golden$|(^|/)(golden|goldens|testdata/golden)/"
    )
    .is_match(path)
}

fn is_check_config(path: &str) -> bool {
    re!(r"(^|/)\.github/workflows/|(^|/)\.gitlab-ci\.ya?ml$|(^|/)\.pre-commit-config\.ya?ml$|(^|/)(pyproject\.toml|setup\.cfg|tox\.ini|pytest\.ini|\.coveragerc|mypy\.ini|ruff\.toml|\.ruff\.toml|\.flake8|\.pylintrc|tsconfig[\w.-]*\.json|\.eslintrc(\.\w+)?|eslint\.config\.\w+|jest\.config\.\w+|vitest\.config\.\w+|clippy\.toml|\.golangci\.ya?ml|codecov\.ya?ml|\.codecov\.ya?ml)$")
        .is_match(path)
}

fn is_dependency_manifest(path: &str) -> bool {
    re!(r"(^|/)(package\.json|package-lock\.json|pnpm-lock\.yaml|yarn\.lock|Cargo\.toml|Cargo\.lock|requirements[\w.-]*\.txt|poetry\.lock|uv\.lock|Pipfile(\.lock)?|go\.mod|go\.sum|Gemfile(\.lock)?|pom\.xml|build\.gradle(\.kts)?)$")
        .is_match(path)
}

/// Suppression markers, with a name for each.
fn suppression_kind(line: &str) -> Option<&'static str> {
    let kinds: [(&'static str, &regex::Regex); 13] = [
        ("noqa", re!(r"#\s*noqa\b")),
        ("type: ignore", re!(r"#\s*type:\s*ignore\b")),
        ("pyright: ignore", re!(r"#\s*pyright:\s*ignore\b")),
        ("pylint: disable", re!(r"#\s*pylint:\s*disable\b")),
        (
            "no cover",
            re!(r"pragma:\s*no\s*cover\b|istanbul ignore|c8 ignore"),
        ),
        ("eslint-disable", re!(r"eslint-disable")),
        ("ts-ignore", re!(r"@ts-(ignore|expect-error|nocheck)\b")),
        ("rust allow", re!(r"#!?\[(allow|expect)\(")),
        ("nolint", re!(r"//\s*(nolint|NOLINT)\b")),
        ("SuppressWarnings", re!(r"@SuppressWarnings\b|@Suppress\(")),
        ("rubocop:disable", re!(r"rubocop:disable\b")),
        ("noinspection", re!(r"noinspection\b")),
        ("fmt: off", re!(r"#\s*fmt:\s*off\b")),
    ];
    kinds
        .iter()
        .find(|(_, re)| re.is_match(line))
        .map(|(k, _)| *k)
}

/// Lines with a suppression in `new` that `old` did not have (one per added copy).
fn suppressions_added(path: &str, old: &str, new: &str) -> Vec<Suppression> {
    let mut before: std::collections::HashMap<&str, usize> = Default::default();
    for l in old.lines().filter(|l| suppression_kind(l).is_some()) {
        *before.entry(l.trim()).or_default() += 1;
    }
    let mut out = Vec::new();
    for (i, l) in new.lines().enumerate() {
        let Some(kind) = suppression_kind(l) else {
            continue;
        };
        match before.get_mut(l.trim()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(Suppression {
                path: path.to_string(),
                line: i + 1,
                kind: kind.into(),
                text: crate::text::truncate_chars(l.trim(), 160),
                has_reason: has_reason(l),
            }),
        }
    }
    out
}

/// The suppression comment explains itself: words after the marker and its codes
/// (`# noqa: BLE001 - one failed unit must not kill a run`), or a Rust `reason = "…"`.
fn has_reason(line: &str) -> bool {
    if re!(r#"reason\s*=\s*""#).is_match(line) {
        return true;
    }
    let after = re!(r"(noqa|type:\s*ignore|pyright:\s*ignore|pylint:\s*disable|no\s*cover|eslint-disable(-next-line|-line)?|@ts-(ignore|expect-error|nocheck)|nolint|NOLINT|rubocop:disable)(.*)$")
        .captures(line)
        .and_then(|c| c.get(c.len() - 1))
        .map_or("", |m| m.as_str());
    // Drop codes (`: E501,F401`, `[arg-type]`, `(lll)`, rule names) and separators.
    let rest = re!(r"^[\s:,\[\]()]*([\w./@-]+[\s,\[\]()]*)*?(?:--?|—|:|#)\s*").replace(after, "");
    rest.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphabetic))
        .count()
        >= 2
}

/// What a change to check config made weaker: lower thresholds, strictness off,
/// failures allowed, rules ignored.
fn config_weaker(old: &str, new: &str) -> Vec<String> {
    let mut out = Vec::new();
    let threshold = re!(
        r#"(?i)\b(fail[_-]under|cov[_-]fail[_-]under|coverageThreshold|branches|lines|functions|statements|min[_-]?coverage|minimum[_-]?coverage|threshold)["']?\s*[=:]\s*([0-9]+(?:\.[0-9]+)?)"#
    );
    let values = |t: &str| {
        let mut m: std::collections::BTreeMap<String, f64> = Default::default();
        for c in threshold.captures_iter(t) {
            if let Ok(v) = c[2].parse::<f64>() {
                let e = m.entry(c[1].to_lowercase()).or_insert(v);
                *e = e.min(v);
            }
        }
        m
    };
    let (o, n) = (values(old), values(new));
    for (k, nv) in &n {
        if let Some(ov) = o.get(k) {
            if nv < ov {
                out.push(format!("{k} {ov} → {nv}"));
            }
        }
    }
    let added = |re: &regex::Regex| re.find_iter(new).count() > re.find_iter(old).count();
    if added(re!(r#"(?i)["']?strict["']?\s*[:=]\s*false"#)) {
        out.push("strict mode off".into());
    }
    if added(re!(r"continue-on-error:\s*true")) {
        out.push("a CI step may fail (continue-on-error)".into());
    }
    if added(re!(r"\|\|\s*true\b")) {
        out.push("`|| true` added".into());
    }
    if added(re!(
        r#"(?i)\b(ignore|extend-ignore|disable|ignore_errors|ignore_missing_imports|skip)\b\s*[=:]"#
    )) {
        out.push("rules ignored or checks skipped".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, old: Option<&str>, new: Option<&str>) -> FileChange {
        FileChange {
            path: path.into(),
            old: old.map(str::to_string),
            new: new.map(str::to_string),
        }
    }

    fn run(changes: &[FileChange]) -> Report {
        audit(changes, &|_| false)
    }

    #[test]
    fn test_files_count_and_flag() {
        let r = run(&[
            change(
                "tests/test_llm.py",
                Some("def test_a():\n    assert a\n\ndef test_b():\n    assert b\n"),
                Some("def test_a():\n    assert a\n"),
            ),
            change(
                "tests/test_new.py",
                None,
                Some("def test_c():\n    assert c\n"),
            ),
            change(
                "tests/test_same.py",
                Some("def test_d():\n    assert d\n"),
                Some("def test_d():\n    assert d  # renamed var\n"),
            ),
        ]);
        assert_eq!(r.tests.len(), 2, "{r:?}");
        let llm = &r.tests[0];
        assert_eq!((llm.assertions, llm.tests), ((2, 1), (2, 1)));
        assert!(llm.weakened.iter().any(|w| w.contains("tests removed")));
        assert_eq!(r.tests[1].status, "added");
        assert_eq!(
            r.one_line(),
            "2 test files changed, 1 test removed (1 may weaken tests)"
        );
        assert_eq!(r.flagged(), 1);
    }

    #[test]
    fn suppressions_with_and_without_reasons() {
        let r = run(&[change(
            "src/run.py",
            Some("import os\nx = 1  # noqa: E501\n"),
            Some("import os  # noqa: F401\nx = 1  # noqa: E501\nexcept Exception:  # noqa: BLE001 - one failed unit must not kill a run\ny = f()  # type: ignore[arg-type]\n"),
        )]);
        let got: Vec<(&str, usize, bool)> = r
            .suppressions
            .iter()
            .map(|s| (s.kind.as_str(), s.line, s.has_reason))
            .collect();
        assert_eq!(
            got,
            [
                ("noqa", 1, false),
                ("noqa", 3, true),
                ("type: ignore", 4, false)
            ]
        );
        let rs = run(&[change(
            "src/lib.rs",
            Some(""),
            Some("#[allow(clippy::too_many_arguments)]\n#[expect(dead_code, reason = \"used by tests only\")]\n// eslint-disable-next-line no-console -- debugging aid for the CLI\n"),
        )]);
        let got: Vec<bool> = rs.suppressions.iter().map(|s| s.has_reason).collect();
        assert_eq!(got, [false, true, true]);
    }

    #[test]
    fn config_and_dependencies() {
        let r = run(&[
            change(
                "pyproject.toml",
                Some("[tool.coverage.report]\nfail_under = 90\n"),
                Some("[tool.coverage.report]\nfail_under = 80\n"),
            ),
            change(
                ".github/workflows/ci.yml",
                Some("    - run: make check\n"),
                Some("    - run: make check\n      continue-on-error: true\n"),
            ),
            change(
                "tsconfig.json",
                Some("{\"strict\": true}"),
                Some("{\"strict\": false}"),
            ),
            change(".eslintrc.json", Some("{}"), Some("{}\n")),
            change("requirements.txt", Some("a\n"), Some("a\nb\n")),
            change("Cargo.lock", Some(""), Some("x")),
        ]);
        let weak: Vec<(&str, &[String])> = r
            .config
            .iter()
            .map(|c| (c.path.as_str(), c.weaker.as_slice()))
            .collect();
        assert_eq!(
            weak[0],
            ("pyproject.toml", &["fail_under 90 → 80".to_string()][..])
        );
        assert!(weak[1].1[0].contains("continue-on-error"));
        assert_eq!(weak[2].1, ["strict mode off".to_string()]);
        assert!(weak[3].1.is_empty());
        assert_eq!(r.dependencies, ["requirements.txt", "Cargo.lock"]);
        assert!(
            r.one_line().contains("3 check configs weakened"),
            "{}",
            r.one_line()
        );
    }

    #[test]
    fn snapshots_and_ignored_paths() {
        let r = audit(
            &[
                change("src/__snapshots__/app.test.ts.snap", Some("a"), Some("b")),
                change("tests/golden/out.txt", Some("a"), Some("b")),
                change("vendor/lib/x_test.go", Some("assert"), None),
            ],
            &|p| p.starts_with("vendor/"),
        );
        assert_eq!(r.snapshots.len(), 2);
        assert!(r.tests.is_empty());
        assert_eq!(r.one_line(), "2 snapshots rewritten");
        assert!(run(&[]).is_empty());
        assert_eq!(run(&[]).one_line(), "no changes to tests or checks");
    }
}
