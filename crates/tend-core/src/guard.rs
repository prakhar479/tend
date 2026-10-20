//! Test guard: rules that detect edits which weaken tests.
//! Rules are language-agnostic and cheap. They give false positives; keep the guard advisory.
//!
//! Every rule reads code only: comments are removed and string literals are replaced by
//! a placeholder first, so `# assert x` and `"assertion"` count for nothing (I2, G1).

use serde::{Deserialize, Serialize};

use crate::broker::Answer;
use crate::text::sha256_hex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardDecision {
    Warn,
    Ask,
    Deny,
}

pub fn is_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    re!(r"(^|/)(tests?|__tests__|spec|specs)/|(^|/)test_[^/]*\.py$|_test\.(py|go|rs)$|\.(test|spec)\.[cm]?[jt]sx?$|(^|/)conftest\.py$|(^|/)tests?\.(py|rs)$|Tests?\.(cs|java|kt)$|_spec\.rb$")
        .is_match(&p)
}

/// The text of an edit holds test code, wherever the file is: Rust unit tests live in
/// `src/`, Go and Python tests can too (G2).
pub fn has_test_code(text: &str) -> bool {
    re!(r"#\[(tokio::)?test\b|#\[cfg\(test\)\]|\bdef\s+test_\w*\s*\(|\bfunc\s+Test\w*\s*\(\s*\w+\s+\*testing\.T\b|@Test\b|\bunittest\.TestCase\b")
        .is_match(text)
}

/// Judge an edit of `path`: a test file, or test code anywhere.
pub fn judges(path: &str, old: &str, new: &str) -> bool {
    is_test_path(path) || has_test_code(old) || has_test_code(new)
}

/// Code without comments, with each string literal replaced by `"s<hash>"`. Distinct
/// strings stay distinct (for the same-operand rule); their contents do not count.
/// Line structure is kept, except inside multi-line strings and comments.
pub fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let at = |i: usize, s: &str| {
        s.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    while i < chars.len() {
        let c = chars[i];
        // Python triple-quoted strings (docstrings).
        if (at(i, "\"\"\"") || at(i, "'''")) && i + 2 < chars.len() {
            let q: String = chars[i..i + 3].iter().collect();
            let mut j = i + 3;
            while j < chars.len() && !at(j, &q) {
                j += 1;
            }
            let body: String = chars[i + 3..j.min(chars.len())].iter().collect();
            out.push_str(&placeholder(&body));
            i = (j + 3).min(chars.len());
            continue;
        }
        match c {
            '"' | '`' | '\'' => {
                // A quote with no closing quote on its line (a Rust lifetime, an apostrophe
                // in prose) is not a string.
                let mut j = i + 1;
                let mut body = String::new();
                let mut closed = false;
                while j < chars.len() {
                    let d = chars[j];
                    if d == '\\' && j + 1 < chars.len() {
                        body.push(chars[j + 1]);
                        j += 2;
                        continue;
                    }
                    if d == c {
                        closed = true;
                        break;
                    }
                    if d == '\n' && c != '`' {
                        break;
                    }
                    body.push(d);
                    j += 1;
                }
                if closed {
                    out.push_str(&placeholder(&body));
                    i = j + 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            '/' if at(i, "//") => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if at(i, "/*") => {
                i += 2;
                while i < chars.len() && !at(i, "*/") {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            // `#` starts a comment, except Rust attributes `#[` and `#![` and C's `#include`.
            '#' if !at(i, "#[") && !at(i, "#![") && !at(i, "#include") => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn placeholder(body: &str) -> String {
    format!("\"s{}\"", &sha256_hex(body)[..8])
}

fn count(re: &regex::Regex, s: &str) -> usize {
    re.find_iter(s).count()
}

/// Compare the old and new text of an edit to a test file.
pub fn assess(old: &str, new: &str) -> Answer {
    let (old, new) = (code_only(old), code_only(new));
    let (old, new) = (old.as_str(), new.as_str());
    // Assertion calls, not mentions: `assert x`, `assert(`, `assertEqual(`, `assert_eq!(`,
    // `assert.Equal(`, `expect(`, `.should`, Go's `t.Error`, testify's `require.X(`.
    let assert_re = re!(
        r"\bassert\s|\bassert\w*\s*[(!]|\bassert\.\w+\s*\(|\bexpect\s*\(|\.should\b|\bt\.(Error|Errorf|Fatal|Fatalf)\(|\brequire\.\w+\(|\bself\.fail\("
    );
    let skip_re = re!(
        r"\bmark\.(skip|skipif|xfail)\b|\bpytest\.(skip|xfail)\(|@(unittest\.)?skip(If|Unless)?\b|\bexpectedFailure\b|\bskipTest\(|\bSkipTest\b|\b(it|test|describe)\.(skip|todo|fails|skipIf)\b|\bx(it|describe|test)\(|#\[ignore\b|\bt\.Skip(Now|f)?\(|@(Disabled|Ignore)\b"
    );
    // Focus: only the marked tests run, so the others stop running.
    let focus_re = re!(r"\b(it|test|describe)\.only\b|\bf(it|describe)\(");
    let panic_re = re!(r"#\[should_panic\b");
    let true_re = re!(r"\|\|\s*true\b");
    let def_re = re!(
        r"\bdef\s+test\w*\s*\(|#\[(tokio::)?test\b|\bfunc\s+Test\w*\s*\(|@Test\b|(^|[^.\w])(it|test)(\.\w+)*\s*\("
    );
    // A handler that turns a failed assertion into a pass.
    let swallow_re = re!(r"\bexcept\s*(\(\s*)?AssertionError\b|\bexcept\s*:");

    let mut reasons = Vec::new();
    let (a_old, a_new) = (count(assert_re, old), count(assert_re, new));
    if a_new < a_old {
        reasons.push(format!("assertions removed ({a_old} → {a_new})"));
    }
    let (d_old, d_new) = (count(def_re, old), count(def_re, new));
    if d_new < d_old {
        reasons.push(format!("tests removed or renamed ({d_old} → {d_new})"));
    }
    if count(skip_re, new) > count(skip_re, old) {
        reasons.push("skip marker added".to_string());
    }
    if count(focus_re, new) > count(focus_re, old) {
        reasons.push("focus marker added (the other tests stop running)".to_string());
    }
    if count(panic_re, new) > count(panic_re, old) {
        reasons.push("`#[should_panic]` added".to_string());
    }
    if count(true_re, new) > count(true_re, old) {
        reasons.push("`|| true` added".to_string());
    }
    if count(swallow_re, new) > count(swallow_re, old) {
        reasons.push("handler that swallows a failed assertion added".to_string());
    }
    if early_returns(new) > early_returns(old) {
        reasons.push("`return` before the assertions added".to_string());
    }
    let always = |s: &str| s.lines().filter(|l| always_passes(l.trim())).count();
    if always(new) > always(old) {
        reasons.push("assertion that always passes added".to_string());
    }
    if let Some(r) = looser_tolerance(old, new) {
        reasons.push(r);
    }
    // TODO(Phase 3): expected value changed to the actual value of the last failure.

    if !reasons.is_empty() {
        Answer::Weakened { reasons }
    } else if a_new > a_old {
        Answer::Strengthened
    } else {
        Answer::Neutral
    }
}

/// An assertion that cannot fail: `assert True`, `assert True, "msg"`, `assert 1 == 1`,
/// `self.assertTrue(True)`, `assert!(true)`, `assert_eq!(1, 1)`, `expect(1).toBe(1)`.
fn always_passes(line: &str) -> bool {
    let literal_true = re!(
        r"^assert\s*\(?\s*(True|true|1|not\s+False)\s*\)?\s*(,.*)?;?$|\bassert(True|_true)?\s*\(\s*(True|true|1)\s*(,[^)]*)?\)|\bassert!\(\s*true\s*(,[^)]*)?\)|\bexpect\(\s*true\s*\)\.toBe(Truthy)?\(\s*(true)?\s*\)"
    );
    if literal_true.is_match(line) {
        return true;
    }
    // The same expression on both sides. The regex crate has no back-references.
    let pairs = [
        re!(r"^assert\s+\(?(.+?)\s*==\s*(.+?)\)?\s*(,.*)?;?$"),
        re!(
            r"\bassert(?:_eq!|Equals?|\.Equal)\s*\(\s*(?:t\s*,\s*)?([^,]+?)\s*,\s*([^,)]+?)\s*[,)]"
        ),
        re!(r"\bexpect\(\s*(.+?)\s*\)\.(?:toBe|toEqual|toStrictEqual)\(\s*(.+?)\s*\)"),
    ];
    pairs.iter().any(|re| {
        re.captures(line)
            .is_some_and(|c| c[1].trim() == c[2].trim() && !c[1].trim().is_empty())
    })
}

/// Bare `return`s followed by an assertion within the next five code lines: the
/// assertions after it no longer run.
fn early_returns(code: &str) -> usize {
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let assert_line = re!(r"\bassert|\bexpect\s*\(|\bt\.(Error|Fatal)");
    lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            re!(r"^return\s*;?$").is_match(l)
                && lines[i + 1..]
                    .iter()
                    .take(5)
                    .take_while(|n| !re!(r"^(def|fn|func|it|test)\b|^\}").is_match(n))
                    .any(|n| assert_line.is_match(n))
        })
        .count()
}

/// A tolerance keyword whose largest value grew (`rel=1e-6` → `rel=0.1`), or a digits
/// argument that shrank (`toBeCloseTo(x, 5)` → `toBeCloseTo(x, 1)`, `places=7` → `2`).
fn looser_tolerance(old: &str, new: &str) -> Option<String> {
    let tol = re!(
        r"\b(rel|abs|rel_tol|abs_tol|atol|rtol|delta|epsilon|tol|tolerance)\s*=\s*([0-9][0-9._eE+-]*)"
    );
    let digits = re!(r"\btoBeCloseTo\([^,()]+,\s*(\d+)\s*\)|\bplaces\s*=\s*(\d+)");
    let max_by_key = |s: &str| {
        let mut m: std::collections::BTreeMap<String, f64> = Default::default();
        for c in tol.captures_iter(s) {
            if let Ok(v) = c[2].replace('_', "").parse::<f64>() {
                let e = m.entry(c[1].to_string()).or_insert(v);
                *e = e.max(v);
            }
        }
        m
    };
    let (o, n) = (max_by_key(old), max_by_key(new));
    for (k, nv) in &n {
        if let Some(ov) = o.get(k) {
            if nv > ov {
                return Some(format!("tolerance raised ({k}: {ov} → {nv})"));
            }
        }
    }
    let min_digits = |s: &str| {
        digits
            .captures_iter(s)
            .filter_map(|c| c.get(1).or(c.get(2)))
            .filter_map(|m| m.as_str().parse::<u32>().ok())
            .min()
    };
    match (min_digits(old), min_digits(new)) {
        (Some(o), Some(n)) if n < o => Some(format!("precision lowered ({o} → {n} digits)")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths() {
        for p in [
            "tests/test_api.py",
            "src/__tests__/a.js",
            "pkg/orders_test.go",
            "web/cart.spec.tsx",
            "conftest.py",
            "test_root.py",
            "crates/x/src/lib_test.rs",
            "specs/cart.js",
            "shop/tests.py",
            "src/tests.rs",
            "Shop.Tests/OrderTests.cs",
            "src/test/java/OrderTest.java",
            "spec/models/order_spec.rb",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/app.py",
            "src/testing.rs",
            "latest/notes.md",
            "src/contest.py",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn test_code_outside_test_paths_is_judged() {
        assert!(judges(
            "src/orders.rs",
            "#[test]\nfn total() { assert_eq!(t(), 6); }",
            ""
        ));
        assert!(judges(
            "app/models.py",
            "def test_total():\n    assert 1\n",
            ""
        ));
        assert!(!judges(
            "src/orders.rs",
            "fn total() -> u32 { 6 }",
            "fn total() -> u32 { 7 }"
        ));
    }

    #[test]
    fn comments_and_strings_do_not_count() {
        let c = code_only(
            "assert x  # assert y\nlet s = \"assert z\"; // expect(w)\n/* assert q */ ok",
        );
        assert_eq!(c.matches("assert").count(), 1, "{c}");
        assert!(c.contains("\"s"), "{c}");
        // Rust attributes and lifetimes survive.
        let c = code_only("#[ignore]\nfn f<'a>(x: &'a str) { assert!(x.len() > 0) }");
        assert!(c.contains("#[ignore]") && c.contains("assert!("), "{c}");
        // Docstrings are strings.
        assert!(!code_only("\"\"\"\nassert x == 1\n\"\"\"\n").contains("assert"));
    }

    fn weakened(old: &str, new: &str) -> bool {
        matches!(assess(old, new), Answer::Weakened { .. })
    }

    /// T1: one weakening and one neutral case per rule and language.
    #[test]
    fn weakening_table() {
        let weak = [
            // Assertions removed or commented out.
            (
                "    assert r.status == 200\n    assert r.json['id'] == 42\n",
                "    assert r.status in (200, 500)\n",
                "assertions removed",
            ),
            (
                "    assert total(cart) == 6\n",
                "    # assert total(cart) == 6\n",
                "assertions removed",
            ),
            (
                "  expect(x).toBe(6);\n",
                "  // expect(x).toBe(6);\n",
                "assertions removed",
            ),
            (
                "    assert_eq!(a, 6);\n",
                "    // assert_eq!(a, 6);\n",
                "assertions removed",
            ),
            (
                "    if got != 6 { t.Errorf(\"got %d\", got) }\n",
                "    _ = got\n",
                "assertions removed",
            ),
            (
                "    assert.Equal(t, 6, got)\n",
                "    _ = got\n",
                "assertions removed",
            ),
            (
                "    expect(a).to.equal(1)\n",
                "    a\n",
                "assertions removed",
            ),
            (
                "    assert x == 1\n",
                "    \"\"\"assert x == 1\"\"\"\n",
                "assertions removed",
            ),
            // Skip markers.
            (
                "def test_a():\n",
                "@pytest.mark.skip\ndef test_a():\n",
                "skip marker",
            ),
            (
                "import pytest\n",
                "import pytest\npytestmark = pytest.mark.skip(reason=\"wip\")\n",
                "skip marker",
            ),
            (
                "def test_a():\n",
                "@mark.skip\ndef test_a():\n",
                "skip marker",
            ),
            (
                "def test_a():\n",
                "@skip('wip')\ndef test_a():\n",
                "skip marker",
            ),
            (
                "    def test_a(self):\n",
                "    @unittest.expectedFailure\n    def test_a(self):\n",
                "skip marker",
            ),
            (
                "    def test_a(self):\n        x()\n",
                "    def test_a(self):\n        self.skipTest('later')\n        x()\n",
                "skip marker",
            ),
            (
                "    x()\n",
                "    raise unittest.SkipTest('x')\n    x()\n",
                "skip marker",
            ),
            (
                "    pytest.param(1, 2),\n",
                "    pytest.param(1, 2, marks=pytest.mark.xfail),\n",
                "skip marker",
            ),
            (
                "    x()\n",
                "    pytest.skip('x')\n    x()\n",
                "skip marker",
            ),
            (
                "#[test]\nfn a() {}\n",
                "#[test]\n#[ignore = \"flaky\"]\nfn a() {}\n",
                "skip marker",
            ),
            (
                "#[test]\nfn a() {}\n",
                "#[test]\n#[ignore]\nfn a() {}\n",
                "skip marker",
            ),
            (
                "#[test]\nfn a() {}\n",
                "#[test]\n#[should_panic]\nfn a() {}\n",
                "should_panic",
            ),
            (
                "func TestA(t *testing.T) {\n",
                "func TestA(t *testing.T) {\n\tt.Skip(\"later\")\n",
                "skip marker",
            ),
            ("it('a', f)", "it.skip('a', f)", "skip marker"),
            (
                "it.each(rows)('a', f)",
                "it.skip.each(rows)('a', f)",
                "skip marker",
            ),
            ("it('a', f)", "it.skipIf(ci)('a', f)", "skip marker"),
            ("test('a', f)", "test.fails('a', f)", "skip marker"),
            ("it('a', f)", "xit('a', f)", "skip marker"),
            (
                "@Test\nvoid a() {}\n",
                "@Test\n@Disabled\nvoid a() {}\n",
                "skip marker",
            ),
            ("it.only('a', f)", "it.skip('a', f)", "skip marker"),
            ("it('a', f)", "it.only('a', f)", "focus marker"),
            // Tests removed or renamed.
            (
                "def test_a():\n    x()\n",
                "def _test_a():\n    x()\n",
                "tests removed",
            ),
            (
                "#[test]\nfn a() { x() }\n",
                "fn a() { x() }\n",
                "tests removed",
            ),
            // Swallowed failures and early returns.
            (
                "    assert f() == 1\n",
                "    try:\n        assert f() == 1\n    except AssertionError:\n        pass\n",
                "swallows",
            ),
            (
                "    assert f() == 1\n",
                "    try:\n        assert f() == 1\n    except:\n        pass\n",
                "swallows",
            ),
            (
                "def test_a():\n    assert f() == 1\n",
                "def test_a():\n    return\n    assert f() == 1\n",
                "`return`",
            ),
            (
                "  expect(f()).toBe(1);\n",
                "  return;\n  expect(f()).toBe(1);\n",
                "`return`",
            ),
            ("run_tests\n", "run_tests || true\n", "|| true"),
            // Always passes.
            (
                "    assert total([1, 2, 3]) == 6",
                "    assert True",
                "always passes",
            ),
            (
                "    assert total([1, 2, 3]) == 6",
                "    assert True, \"todo\"",
                "always passes",
            ),
            (
                "    assert total([1, 2, 3]) == 6",
                "    assert 1 == 1",
                "always passes",
            ),
            (
                "    self.assertEqual(total(c), 6)",
                "    self.assertTrue(True)",
                "always passes",
            ),
            (
                "    assert total([1, 2, 3]) == 6",
                "    assert not False",
                "always passes",
            ),
            (
                "    assert_eq!(total(c), 6);",
                "    assert_eq!(1, 1);",
                "always passes",
            ),
            (
                "    assert_eq!(total(c), 6);",
                "    assert!(true, \"x\");",
                "always passes",
            ),
            (
                "    expect(total(c)).toBe(6);",
                "    expect(1).toBe(1);",
                "always passes",
            ),
            (
                "    expect(total(c)).toBe(6);",
                "    expect(true).toBeTruthy()",
                "always passes",
            ),
            (
                "    assert got == want",
                "    assert got == got",
                "always passes",
            ),
            // Looser tolerances.
            (
                "    assert x == pytest.approx(0.3, rel=1e-6)",
                "    assert x == pytest.approx(0.3, rel=0.1)",
                "tolerance raised",
            ),
            (
                "    expect(x).toBeCloseTo(0.3, 5);",
                "    expect(x).toBeCloseTo(0.3, 1);",
                "precision lowered",
            ),
            (
                "    self.assertAlmostEqual(x, 0.3, places=7)",
                "    self.assertAlmostEqual(x, 0.3, places=2)",
                "precision lowered",
            ),
            (
                "    assert abs(x - y) < tol, (tol=1e-9)",
                "    assert abs(x - y) < tol, (tol=1)",
                "tolerance raised",
            ),
        ];
        for (old, new, why) in weak {
            let reasons = match assess(old, new) {
                Answer::Weakened { reasons } => reasons,
                other => panic!("not flagged ({other:?}):\n{old}\n→\n{new}"),
            };
            assert!(
                reasons.iter().any(|r| r.contains(why)),
                "expected `{why}`, got {reasons:?}:\n{old}\n→\n{new}"
            );
        }
        let neutral = [
            // Refactors, rewording, imports.
            (
                "from pandas.testing import assert_frame_equal\nassert_frame_equal(a, b)\n",
                "assert_frame_equal(a, b)\n",
            ),
            (
                "# check the assertion holds\nassert x\n",
                "# the assertion must hold\nassert x\n",
            ),
            ("assert a == 1", "assert a == 2"),
            (
                "    assert r.json['msg'] == \"assert ok\"\n",
                "    assert r.json['msg'] == \"assert fine\"\n",
            ),
            (
                "def test_a():\n    x()\n",
                "def test_a_renamed_clearly():\n    x()\n",
            ),
            ("it('a', f)", "it('a better name', f)"),
            ("    assert ok is True", "    assert ok is True and done"),
            ("    assert y", "    assert flag == True_value"),
            ("    assert y", "assert x"),
            (
                "    assert x == pytest.approx(0.3, rel=0.1)",
                "    assert x == pytest.approx(0.3, rel=1e-6)",
            ),
            (
                "    if x:\n        return\n    do()\n",
                "    if x:\n        return\n    do_more()\n",
            ),
            ("fn helper() -> u32 { 6 }\n", "fn helper() -> u32 { 7 }\n"),
        ];
        for (old, new) in neutral {
            assert!(
                !weakened(old, new),
                "flagged:\n{old}\n→\n{new}\n{:?}",
                assess(old, new)
            );
        }
    }

    #[test]
    fn neutral_and_strengthened() {
        assert_eq!(assess("assert a == 1", "assert a == 2"), Answer::Neutral);
        assert_eq!(
            assess("assert a", "assert a\nassert b"),
            Answer::Strengthened
        );
    }
}
