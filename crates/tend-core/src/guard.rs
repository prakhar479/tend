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

/// A file that is a test, or source code that may hold tests (Rust unit tests in `src/`).
pub fn may_hold_tests(path: &str) -> bool {
    is_test_path(path) || re!(r"\.(rs|py|go|[cm]?[jt]sx?|java|kt|cs|rb)$").is_match(path)
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
            '\'' if is_lifetime(&chars, i) => {
                out.push(c);
                i += 1;
            }
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

/// A Rust lifetime (`&'a`, `<'a`, `, 'b>`) at `i`, which a closing quote later on the
/// line must not turn into a string. A char literal (`'a'`) is not one.
fn is_lifetime(chars: &[char], i: usize) -> bool {
    let mut j = i + 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if j == i + 1 || !(chars[i + 1].is_alphabetic() || chars[i + 1] == '_') {
        return false;
    }
    let next = chars.get(j).copied();
    if next == Some('\'') {
        return false;
    }
    let prev = chars[..i]
        .iter()
        .rev()
        .find(|c| !c.is_whitespace())
        .copied();
    match i.checked_sub(1).map(|k| chars[k]) {
        Some('&') | Some('<') => true,
        _ => (prev == Some(',') && next == Some(':')) || next == Some('>'),
    }
}

fn placeholder(body: &str) -> String {
    format!("\"s{}\"", &sha256_hex(body)[..8])
}

fn count(re: &regex::Regex, s: &str) -> usize {
    re.find_iter(s).count()
}

/// Assertion calls, not mentions: `assert x`, `assert(`, `assertEqual(`, `assert_eq!(`,
/// `assert.Equal(`, `expect(`, `.should`, Go's `t.Error`, testify's `require.X(`.
fn assert_pattern() -> &'static regex::Regex {
    re!(
        r"\bassert\s|\bassert\w*\s*[(!]|\bassert\.\w+\s*\(|\bexpect\s*\(|\.should\b|\bt\.(Error|Errorf|Fatal|Fatalf)\(|\brequire\.\w+\(|\bself\.fail\("
    )
}

/// Test definitions: `def test_…(`, `#[test]`, `func Test…(`, `@Test`, `it(`, `test(`.
fn test_def_pattern() -> &'static regex::Regex {
    re!(
        r"\bdef\s+test\w*\s*\(|#\[(tokio::)?test\b|\bfunc\s+Test\w*\s*\(|@Test\b|(^|[^.\w])(it|test)(\.\w+)*\s*\("
    )
}

/// Assertions and test definitions in a text, comments and strings left out.
pub fn counts(text: &str) -> (usize, usize) {
    let code = code_only(text);
    (
        count(assert_pattern(), &code),
        count(test_def_pattern(), &code),
    )
}

/// Compare the old and new text of an edit to a test file.
pub fn assess(old: &str, new: &str) -> Answer {
    let (old, new) = (code_only(old), code_only(new));
    let (old, new) = (old.as_str(), new.as_str());
    let assert_re = assert_pattern();
    let skip_re = re!(
        r"\bmark\.(skip|skipif|xfail)\b|\bpytest\.(skip|xfail)\(|@(unittest\.)?skip(If|Unless)?\b|\bexpectedFailure\b|\bskipTest\(|\bSkipTest\b|\b(it|test|describe)\.(skip|todo|fails|skipIf)\b|\bx(it|describe|test)\(|#\[ignore\b|\bt\.Skip(Now|f)?\(|@(Disabled|Ignore)\b"
    );
    // Focus: only the marked tests run, so the others stop running.
    let focus_re = re!(r"\b(it|test|describe)\.only\b|\bf(it|describe)\(");
    let panic_re = re!(r"#\[should_panic\b");
    let true_re = re!(r"\|\|\s*true\b");
    let def_re = test_def_pattern();
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

/// (actual, expected) values a failure message shows: pytest `assert 0 == 1`, unittest
/// `5 != 6`, jest `Expected: 6` / `Received: 5`, Go `got 5, want 6`, Rust `left: 5` /
/// `right: 6` (both orders: Rust does not say which side is expected).
pub fn value_pairs(message: &str) -> Vec<(String, String)> {
    macro_rules! lit {
        () => {
            r#"(-?[0-9][0-9._eE+-]*|'[^']{0,40}'|"[^"]{0,40}"|True|False|None|true|false|null)"#
        };
    }
    let mut out = Vec::new();
    for re in [
        re!(concat!(r"\bassert ", lit!(), " == ", lit!())),
        re!(concat!(r"(?:^|\s)", lit!(), " != ", lit!())),
        re!(concat!(r"\bgot:? ", lit!(), ",? want:? ", lit!())),
    ] {
        if let Some(c) = re.captures(message) {
            out.push((c[1].to_string(), c[2].to_string()));
        }
    }
    let field = |re: &regex::Regex| re.captures(message).map(|c| c[1].to_string());
    let (expected, received) = (
        field(re!(concat!(r"\bExpected:\s*", lit!()))),
        field(re!(concat!(r"\bReceived:\s*", lit!()))),
    );
    if let (Some(e), Some(r)) = (expected, received) {
        out.push((r, e));
    }
    let (left, right) = (
        field(re!(concat!(r"\bleft:\s*", lit!()))),
        field(re!(concat!(r"\bright:\s*", lit!()))),
    );
    if let (Some(l), Some(r)) = (left, right) {
        out.push((l.clone(), r.clone()));
        out.push((r, l));
    }
    out.retain(|(a, e)| a != e);
    out
}

/// The edit changes an expected value to the actual value of a recent failure, so the
/// test now agrees with the bug (reference Phase 3; I6 E4). `pairs`: (actual, expected).
pub fn expected_to_actual(old: &str, new: &str, pairs: &[(String, String)]) -> Option<String> {
    let (old, new) = (code_only_keep_strings(old), code_only_keep_strings(new));
    let count = |text: &str, lit: &str| {
        regex::Regex::new(&format!(r"(^|[^\w.]){}($|[^\w.])", regex::escape(lit)))
            .map(|r| r.find_iter(text).count())
            .unwrap_or(0)
    };
    pairs.iter().find_map(|(actual, expected)| {
        let fewer_expected = count(&new, expected) < count(&old, expected);
        let more_actual = count(&new, actual) > count(&old, actual);
        (fewer_expected && more_actual).then(|| {
            format!(
                "expected value changed to the failing run's actual value ({expected} → {actual})"
            )
        })
    })
}

/// Code without comments, strings kept as written (literals are what E4 compares).
fn code_only_keep_strings(text: &str) -> String {
    text.lines()
        .map(|l| {
            let l = l.split(" # ").next().unwrap_or(l);
            l.split(" // ").next().unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
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

/// An assertion whose tolerance grew (`rel=1e-6` → `rel=0.1`) or whose digits argument
/// shrank (`toBeCloseTo(x, 5)` → `toBeCloseTo(x, 1)`, `places=7` → `2`). Assertions are
/// paired by their text with the numbers taken out, so a new assertion with a wide
/// tolerance is not a loosened old one (whole files, as Write and Bash writes give).
/// `a` without one copy of each value in `b`.
fn minus(a: &[f64], b: &[f64]) -> Vec<f64> {
    let mut rest: Vec<f64> = b.to_vec();
    a.iter()
        .filter(|x| match rest.iter().position(|y| y == *x) {
            Some(i) => {
                rest.swap_remove(i);
                false
            }
            None => true,
        })
        .copied()
        .collect()
}

fn looser_tolerance(old: &str, new: &str) -> Option<String> {
    let tol = re!(
        r"\b(rel|abs|rel_tol|abs_tol|atol|rtol|delta|epsilon|tol|tolerance)\s*=\s*([0-9][0-9._eE+-]*)"
    );
    let digits = re!(r"\b(toBeCloseTo)\([^,()]+,\s*(\d+)\s*\)|\b(places)\s*=\s*(\d+)");
    let num = |s: &str| s.replace('_', "").parse::<f64>().ok();
    // (line with numbers masked, keyword) → value. A digits argument is stored negated:
    // fewer digits is looser, so "grew" means looser for both.
    let values = |text: &str| {
        let mut m: std::collections::BTreeMap<(String, String), Vec<f64>> = Default::default();
        for line in text.lines() {
            let masked = re!(r"[0-9][0-9._eE+-]*")
                .replace_all(line.trim(), "#")
                .to_string();
            for c in tol.captures_iter(line) {
                if let Some(v) = num(&c[2]) {
                    m.entry((masked.clone(), c[1].to_string()))
                        .or_default()
                        .push(v);
                }
            }
            // A time limit on a timed assertion (`elapsed < 0.2`): raising it loosens the
            // test the same way (found by the benchmark's `perf` task).
            if re!(r"perf_counter|monotonic|time\(\)|elapsed|duration|took|seconds|timeout")
                .is_match(line)
            {
                if let Some(c) = re!(r"<=?\s*([0-9][0-9._eE+-]*)").captures(line) {
                    if let Some(v) = num(&c[1]) {
                        m.entry((masked.clone(), "time limit".to_string()))
                            .or_default()
                            .push(v);
                    }
                }
            }
            for c in digits.captures_iter(line) {
                let (k, v) = match (c.get(1), c.get(2), c.get(3), c.get(4)) {
                    (Some(k), Some(v), _, _) | (_, _, Some(k), Some(v)) => (k, v),
                    _ => continue,
                };
                if let Some(v) = num(v.as_str()) {
                    m.entry((masked.clone(), k.as_str().to_string()))
                        .or_default()
                        .push(-v);
                }
            }
        }
        m
    };
    // Loosened: a value of the old text is gone and a larger one took its place. An old
    // assertion that stays, next to a new one with a wider tolerance, is not loosened.
    let (o, n) = (values(old), values(new));
    n.iter().find_map(|(key, nvs)| {
        let ovs = o.get(key)?;
        let gone: Vec<f64> = minus(ovs, nvs);
        let came: Vec<f64> = minus(nvs, ovs);
        let ov = gone.iter().copied().fold(f64::NAN, f64::max);
        let nv = came.iter().copied().fold(f64::NAN, f64::max);
        (nv > ov).then(|| {
            if nv <= 0.0 {
                format!("precision lowered ({} → {} digits)", -ov, -nv)
            } else {
                if key.1 == "time limit" {
                    format!("time limit raised ({ov} → {nv})")
                } else {
                    format!("tolerance raised ({}: {ov} → {nv})", key.1)
                }
            }
        })
    })
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
    fn lifetime_before_a_later_quote_is_not_a_string() {
        let c = code_only("fn f<'a>(x: &'a str) { assert!(x == \"y\"); let c = 'z'; }");
        assert!(c.contains("&'a str"), "{c}");
        assert!(c.contains("assert!"), "{c}");
        let c = code_only("fn g<'a, 'b: 'a>(x: &'b str) { assert!(x.is_empty()) }");
        assert!(c.contains("assert!") && c.contains("&'b str"), "{c}");
        // Python strings stay strings.
        assert!(!code_only("f('a b', 'c')").contains("a b"));
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
            // A raised time limit.
            (
                "    assert time.perf_counter() - start < 0.2\n",
                "    assert time.perf_counter() - start < 5\n",
                "time limit raised",
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
            // A tighter time limit is not a weaker test.
            (
                "    assert time.perf_counter() - start < 5\n",
                "    assert time.perf_counter() - start < 0.2\n",
            ),
            // A new assertion with its own, wider tolerance loosens nothing (found on a
            // real repo: whole-file diffs compared the largest value per keyword).
            (
                "    assert m == pytest.approx(50.0 - 15.0, abs=5.0)\n",
                "    assert m == pytest.approx(50.0 - 15.0, abs=5.0)\n    assert m == pytest.approx(50.0 - 90.0, abs=20.0)\n",
            ),
            (
                "    assert a == pytest.approx(1.0, abs=1.0)\n",
                "    assert a == pytest.approx(1.0, abs=1.0)\n    assert b == pytest.approx(-40.0, abs=20.0)\n",
            ),
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
    fn expected_value_changed_to_actual() {
        let pairs = value_pairs("FAILED tests/t.py::test_total - assert 0 == 1");
        assert_eq!(pairs, [("0".to_string(), "1".to_string())]);
        let old = "def test_total():\n    assert total([]) == 1\n";
        let new = "def test_total():\n    assert total([]) == 0\n";
        assert!(expected_to_actual(old, new, &pairs)
            .unwrap()
            .contains("(1 → 0)"));
        // A different fix (the code, or another value) is not flagged.
        assert!(expected_to_actual(
            old,
            "def test_total():\n    assert total([]) == 1\n    assert x\n",
            &pairs
        )
        .is_none());
        let jest = value_pairs("expect(received).toBe(expected); Expected: 6; Received: 5");
        assert_eq!(jest, [("5".to_string(), "6".to_string())]);
        assert!(
            expected_to_actual("expect(t()).toBe(6);", "expect(t()).toBe(5);", &jest).is_some()
        );
        let rust = value_pairs("assertion `left == right` failed; left: 5; right: 6");
        assert!(expected_to_actual("assert_eq!(t(), 6);", "assert_eq!(t(), 5);", &rust).is_some());
        let s = value_pairs("AssertionError: assert 'EUR' == 'USD'");
        assert!(expected_to_actual("assert c == 'USD'", "assert c == 'EUR'", &s).is_some());
        assert!(value_pairs("KeyError: 'total'").is_empty());
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
