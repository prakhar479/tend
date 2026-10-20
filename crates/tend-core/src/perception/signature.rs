//! Error signatures: decide "same failure" without a model.
//!
//! A signature is a hash of (test id, normalized message). Normalization removes
//! details that change between runs: paths, line numbers, addresses, timestamps,
//! durations, temporary names.

use std::collections::{HashMap, HashSet};

use crate::text::{clean_lines, sha256_hex, strip_ansi, truncate_chars};

const MAX_FAILURES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub signature: String,
    pub test: Option<String>,
    /// Raw message (trimmed). Normalize it only for hashing.
    pub message: String,
    /// Only the runner's summary line was seen (`… | tail -1`): the run failed, but which
    /// tests is unknown.
    pub summary_only: bool,
}

impl Failure {
    /// Short human-readable description.
    pub fn summary(&self) -> String {
        let s = match (&self.test, self.message.is_empty()) {
            (Some(t), false) => format!("{t}: {}", self.message),
            (Some(t), true) => t.clone(),
            (None, _) => self.message.clone(),
        };
        truncate_chars(&s, 160)
    }
}

pub fn normalize_message(msg: &str) -> String {
    let s = strip_ansi(msg);
    let s = re!(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
        .replace_all(&s, "<uuid>");
    let s = re!(r"0x[0-9a-fA-F]+").replace_all(&s, "<addr>");
    let s = re!(r"\bid='?\d+'?").replace_all(&s, "id=<n>");
    let s = re!(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
        .replace_all(&s, "<ts>");
    // Timings, not values: `0.12s`, `340ms`, `3 seconds`. A bare `5s` may be a value.
    let s = re!(r"\b\d+\.\d+\s?s\b|\b\d+(\.\d+)?\s?(ms|sec|seconds)\b").replace_all(&s, "<dur>");
    // Machine-specific locations: a file path (with an extension) keeps its name; a
    // directory under a temp or home root goes. `/api/v1/orders` and `1/3` are values.
    let s = re!(r"(?:[A-Za-z]:)?(?:[\w.~-]*[/\\])+([\w-]+\.[A-Za-z]\w{0,4})\b")
        .replace_all(&s, "<path>/$1");
    let s = re!(r"(?:/private)?/(?:tmp|var/folders|var/tmp|home|Users)/[\w./~-]*")
        .replace_all(&s, "<path>");
    // Line and column right after a file name: `x.py:12`, `x.go:3:4`, `X.cs(12,5)`.
    let s = re!(r"(\.[A-Za-z]\w{0,4}):\d+(:\d+)?\b").replace_all(&s, "$1:<n>");
    let s = re!(r"(\.[A-Za-z]\w{0,4})\(\d+,\s*\d+\)").replace_all(&s, "$1(<n>)");
    let s = re!(r"\bline \d+").replace_all(&s, "line <n>");
    let s = re!(r"\bdue to \d+ previous").replace_all(&s, "due to <n> previous");
    // Temporary names carry a random part with a digit: `tmpa8f2k`, `pytest-12`.
    let s = re!(r"\b(tmp|temp|pytest-of)[\w-]*\d[\w-]*").replace_all(&s, "<tmp>");
    let s = re!(r"\s+").replace_all(&s, " ");
    s.trim().to_string()
}

pub fn signature_of(test: Option<&str>, message: &str) -> String {
    let key = format!("{}|{}", test.unwrap_or(""), normalize_message(message));
    sha256_hex(&key)[..16].to_string()
}

/// Find failures in tool output. Runner-specific patterns first, generic errors as fallback.
/// The order of the result follows the output. Duplicates (same signature) are removed.
pub fn extract_failures(output: &str) -> Vec<Failure> {
    let lines: Vec<String> = clean_lines(output)
        .into_iter()
        .map(|l| strip_grep_prefix(&l).to_string())
        .collect();
    let panics = cargo_panics(&lines);
    let mut found: Vec<(Option<String>, String)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        // pytest short summary: FAILED tests/test_api.py::test_get - AssertionError: ...
        // A parametrized id may hold spaces (`test_x[a b]`); unittest's `FAILED (failures=1)`
        // is a summary, not a test.
        if let Some(c) = re!(r"^(?:FAILED|ERROR) ([^(\s].*?)(?: - (.*))?$").captures(line) {
            let msg = c.get(2).map_or("", |m| m.as_str()).trim().to_string();
            found.push((Some(c[1].trim().to_string()), msg));
        // unittest: FAIL: test_total (tests.test_cart.CartTest[.test_total])
        } else if let Some(c) = re!(r"^(?:FAIL|ERROR): (\w+) \(([\w.]+)\)").captures(line) {
            let (name, class) = (&c[1], &c[2]);
            let test = if class.ends_with(&format!(".{name}")) {
                class.to_string()
            } else {
                format!("{class}.{name}")
            };
            found.push((Some(test), unittest_message(&lines, i)));
        // cargo test: test orders::tests::total ... FAILED
        } else if let Some(c) = re!(r"^test (\S+) \.\.\. FAILED$").captures(line) {
            let t = c[1].to_string();
            let msg = panics.get(&t).cloned().unwrap_or_default();
            found.push((Some(t), msg));
        // go test: --- FAIL: TestX (0.00s)
        } else if let Some(c) = re!(r"^\s*--- FAIL: (\S+)").captures(line) {
            let name = c[1].to_string();
            let msg = go_message(&lines, i, &name);
            found.push((Some(name), msg));
        // jest: ● Suite › test name
        } else if let Some(c) = re!(r"^\s*● (.+)$").captures(line) {
            // `● Validation Warning:`, `● Deprecation Warning:`, `● Console` are not failures.
            let what = c[1].trim();
            if !re!(r"^(Validation|Deprecation) Warning\b|^Console\b").is_match(what) {
                found.push((Some(what.to_string()), jest_message(&lines, i)));
            }
        // vitest: " FAIL  file > suite > test", message on the next line
        } else if let Some(c) = re!(r"^\s*FAIL\s+(\S.* > .+)$").captures(line) {
            found.push((
                Some(c[1].trim().to_string()),
                next_nonempty(&lines, i).unwrap_or_default(),
            ));
        }
    }
    // go reports a parent test as failed when a subtest fails. Keep the subtest only.
    let names: Vec<String> = found.iter().filter_map(|f| f.0.clone()).collect();
    found.retain(|(t, _)| {
        t.as_deref().is_none_or(|t| {
            let prefix = format!("{t}/");
            !names.iter().any(|n| n.starts_with(&prefix))
        })
    });

    // Generic error lines are failures only in a run that did not pass: `pytest -s` and
    // `cargo test -- --nocapture` print logged errors from green tests (I3, L1).
    if found.is_empty() && !run_passed(&lines) {
        for line in &lines {
            let l = line.trim();
            // pytest's `E   KeyError: 'total'` detail lines, when the agent cut the summary.
            let l = match l.strip_prefix("E ") {
                Some(rest) if re!(r"^\s*(\w+(Error|Exception)\b|assert\b)").is_match(rest) => {
                    rest.trim()
                }
                _ => l,
            };
            // Cargo's closing lines (`could not compile …`, `aborting due to …`) only
            // count the errors above them: as a signature they would make every build
            // failure look like the same one.
            let summary = re!(r"^error: (could not compile|aborting due to)\b").is_match(l);
            if !summary && re!(r"^\w+(Error|Exception)(: .+)?$|^assert\s.+|^error(\[E\d+\])?: .+|\berror TS\d+: .+|:\d+:\d+: error: .+")
                .is_match(l)
            {
                found.push((None, l.to_string()));
            }
        }
    }
    let mut summary_only = false;
    // Only a summary line is left (`… | tail -1`). It still says the run failed, and the
    // same counts after an edit are the same failure (R1.2).
    if found.is_empty() {
        let summary = lines
            .iter()
            .rev()
            .map(|l| l.trim().trim_matches('=').trim())
            .find(|l| {
                re!(r"^\d+ failed\b|^Tests:\s+\d+ failed\b|^test result: FAILED\b").is_match(l)
            });
        if let Some(l) = summary {
            found.push((None, l.to_string()));
            summary_only = true;
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (test, message) in found {
        let signature = signature_of(test.as_deref(), &message);
        if seen.insert(signature.clone()) {
            out.push(Failure {
                signature,
                test,
                message,
                summary_only,
            });
            if out.len() >= MAX_FAILURES {
                break;
            }
        }
    }
    out
}

/// The runner said the run passed, and nothing said it failed.
fn run_passed(lines: &[String]) -> bool {
    let failed = re!(
        r"\b[1-9]\d* (failed|errors?)\b|^test result: FAILED\b|^FAIL(ED)?\b|^Tests:.*\bfailed\b|^FAILED \("
    );
    let passed = re!(
        r"\b\d+ passed\b|^test result: ok\b|^ok\s+\S|^OK( \(.*\))?$|^Tests:\s+\d+ passed\b|^PASS\b"
    );
    let trimmed = || lines.iter().map(|l| l.trim().trim_matches('=').trim());
    !trimmed().any(|l| failed.is_match(l)) && trimmed().any(|l| passed.is_match(l))
}

/// unittest: the block after `FAIL: …` runs to the next `----`/`====` rule; its last
/// `XxxError: …` line is the message.
fn unittest_message(lines: &[String], i: usize) -> String {
    let rule = |l: &str| re!(r"^(-{20,}|={20,})$").is_match(l.trim());
    let rest = &lines[(i + 1).min(lines.len())..];
    let start = usize::from(rest.first().is_some_and(|l| rule(l)));
    rest[start..]
        .iter()
        .take_while(|l| !rule(l))
        .filter(|l| re!(r"^\w+(Error|Exception)\b").is_match(l.trim()))
        .last()
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

/// Map cargo test thread names to panic messages. Handles both formats:
/// `panicked at 'msg', src/x.rs:1:2` and `panicked at src/x.rs:1:2:` + message on the next line.
fn cargo_panics(lines: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        // Newer Rust adds the thread id: `thread 'name' (16652) panicked at src/x.rs:1:2:`.
        if let Some(c) = re!(r"^thread '([^']+)'(?: \(\d+\))? panicked at (.*)$").captures(l) {
            let rest = c[2].trim();
            let msg = if rest.ends_with(':') {
                // assert_eq! puts the values on `left:`/`right:` lines; they tell failures apart.
                let mut parts: Vec<String> = next_nonempty(lines, i).into_iter().collect();
                parts.extend(
                    lines
                        .iter()
                        .skip(i + 2)
                        .take(2)
                        .map(|l| l.trim().to_string())
                        .filter(|l| l.starts_with("left:") || l.starts_with("right:")),
                );
                parts.join("; ")
            } else {
                rest.to_string()
            };
            m.entry(c[1].to_string()).or_insert(msg);
        }
    }
    m
}

/// go test message for the failure at line `i`. With `-v`, test logs come before the
/// `--- FAIL` line, inside the test's `=== RUN` block. Without `-v`, they come after it.
fn go_message(lines: &[String], i: usize, name: &str) -> String {
    let is_marker = |l: &str| {
        let t = l.trim_start();
        t.starts_with("=== ") || t.starts_with("--- ")
    };
    let run = format!("=== RUN   {name}");
    if let Some(r) = lines[..i].iter().rposition(|l| l.trim() == run) {
        let logged = lines[r + 1..i]
            .iter()
            .take_while(|l| !is_marker(l))
            .find(|l| !l.trim().is_empty());
        if let Some(l) = logged {
            return l.trim().to_string();
        }
    }
    match next_nonempty(lines, i) {
        Some(l) if !is_marker(&l) && !re!(r"^(FAIL|ok)\s").is_match(&l) => l,
        _ => String::new(),
    }
}

/// jest message: the first line after `●`, plus the values that make this failure
/// different from another one (`Expected:`/`Received:` lines, or diff lines).
fn jest_message(lines: &[String], i: usize) -> String {
    let Some(start) = lines
        .iter()
        .skip(i + 1)
        .position(|l| !l.trim().is_empty())
        .map(|p| p + i + 1)
    else {
        return String::new();
    };
    let mut parts = vec![lines[start].trim().to_string()];
    for l in &lines[start + 1..] {
        let t = l.trim();
        // The code frame, the stack, or the next failure ends the message.
        if re!(r"^>?\s*\d+ \|").is_match(t) || t.starts_with("at ") || t.starts_with("● ") {
            break;
        }
        let value = re!(r"^(Expected|Received):").is_match(t)
            || (re!(r"^[-+] ").is_match(t) && !re!(r"^[-+] (Expected|Received)\b").is_match(t));
        if value && parts.len() < 7 {
            parts.push(t.to_string());
        }
    }
    parts.join("; ")
}

/// `grep -n` output: `83:--- FAIL: TestX`. Drop the line number when a known failure
/// line follows it.
fn strip_grep_prefix(line: &str) -> &str {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || !matches!(line.as_bytes().get(digits), Some(b':' | b'-')) {
        return line;
    }
    let rest = &line[digits + 1..];
    let t = rest.trim_start();
    let known = [
        "--- FAIL", "FAILED ", "ERROR ", "FAIL", "test ", "● ", "E ", "thread '",
    ];
    if known.iter().any(|k| t.starts_with(k)) {
        rest
    } else {
        line
    }
}

fn next_nonempty(lines: &[String], i: usize) -> Option<String> {
    lines
        .iter()
        .skip(i + 1)
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYTEST: &str = include_str!("../../../../fixtures/outputs/pytest_fail.txt");
    const CARGO: &str = include_str!("../../../../fixtures/outputs/cargo_fail.txt");
    const GO: &str = include_str!("../../../../fixtures/outputs/go_test_fail.txt");
    const GO_V: &str = include_str!("../../../../fixtures/outputs/go_test_v_fail.txt");
    const JEST: &str = include_str!("../../../../fixtures/outputs/jest_fail.txt");
    const VITEST: &str = include_str!("../../../../fixtures/outputs/vitest_fail.txt");

    fn pairs(f: &[Failure]) -> Vec<(&str, &str)> {
        f.iter()
            .map(|f| (f.test.as_deref().unwrap_or(""), f.message.as_str()))
            .collect()
    }

    #[test]
    fn go_failures_plain_and_verbose() {
        let want = vec![
            (
                "TestTotalWithTax",
                "orders_test.go:11: Total() = 1998, want 2158",
            ),
            (
                "TestFind/id=42",
                "orders_test.go:20: Find(42): order 42 not found",
            ),
            (
                "TestPanics",
                "panic: assignment to entry in nil map [recovered]",
            ),
        ];
        let plain = extract_failures(GO);
        let verbose = extract_failures(GO_V);
        assert_eq!(pairs(&plain), want);
        assert_eq!(pairs(&verbose), want);
        let sigs = |f: &[Failure]| f.iter().map(|f| f.signature.clone()).collect::<Vec<_>>();
        assert_eq!(sigs(&plain), sigs(&verbose), "same failure, same signature");
    }

    #[test]
    fn jest_failures_keep_expected_and_received() {
        assert_eq!(
            pairs(&extract_failures(JEST)),
            vec![
                (
                    "orders › total includes tax",
                    "expect(received).toBe(expected) // Object.is equality; Expected: 2158; Received: 1998"
                ),
                (
                    "orders › find order 42",
                    "TypeError: Cannot read properties of undefined (reading 'total')"
                ),
                (
                    "orders › find returns an object",
                    "expect(received).toEqual(expected) // deep equality; -   \"id\": 8,; +   \"id\": 7,"
                ),
            ]
        );
    }

    #[test]
    fn vitest_failures() {
        assert_eq!(
            pairs(&extract_failures(VITEST)),
            vec![
                (
                    "orders.test.mjs > orders > total includes tax",
                    "AssertionError: expected 1998 to be 2158 // Object.is equality"
                ),
                (
                    "orders.test.mjs > orders > find order 42",
                    "TypeError: Cannot read properties of undefined (reading 'total')"
                ),
                (
                    "orders.test.mjs > orders > find returns an object",
                    "AssertionError: expected { id: 7 } to deeply equal { id: 8 }"
                ),
            ]
        );
    }

    #[test]
    fn normalization_ignores_volatile_details() {
        let a = "AssertionError at /home/ana/shop/src/app.py:42:7 (0x7ffd5e1c) after 1.21s";
        let b = "AssertionError at /tmp/ci/shop/src/app.py:57:3 (0x55aa01b2) after 0.98s";
        assert_eq!(signature_of(None, a), signature_of(None, b));
    }

    #[test]
    fn different_values_are_different_failures() {
        assert_ne!(
            signature_of(Some("t"), "assert 500 == 200"),
            signature_of(Some("t"), "assert 404 == 200")
        );
    }

    #[test]
    fn pytest_failures() {
        let f = extract_failures(PYTEST);
        assert_eq!(f.len(), 2);
        assert_eq!(
            f[0].test.as_deref(),
            Some("tests/test_api.py::test_get_order")
        );
        assert_eq!(f[0].message, "assert 500 == 200");
    }

    #[test]
    fn cargo_failures_with_panic_messages() {
        // Real cargo 1.95 output: panic lines carry a thread id, `thread 'x' (16652) panicked`.
        let found = extract_failures(CARGO);
        let mut got = pairs(&found);
        got.sort();
        assert_eq!(
            got,
            vec![
                (
                    "orders::tests::rejects_zero_qty",
                    "expected Err(InvalidQty), got Ok(Order { id: 7, qty: 0 })"
                ),
                (
                    "orders::tests::total_includes_tax",
                    "assertion `left == right` failed; left: 1998; right: 2158"
                ),
            ]
        );
        // The older format without a thread id, and the message on the same line.
        let old = "test a::b ... FAILED\nthread 'a::b' panicked at 'boom', src/x.rs:1:2\n";
        assert_eq!(
            pairs(&extract_failures(old)),
            vec![("a::b", "'boom', src/x.rs:1:2")]
        );
    }

    /// R1.2: the agent trims test output (99% of runs). Failures still count.
    #[test]
    fn trimmed_outputs() {
        let e_lines = include_str!("../../../../fixtures/outputs/pytest_grep_e_lines.txt");
        assert_eq!(
            pairs(&extract_failures(e_lines)),
            vec![("", "assert 500 == 200"), ("", "KeyError: 'total'")]
        );
        let summary = include_str!("../../../../fixtures/outputs/pytest_summary_only.txt");
        assert_eq!(
            pairs(&extract_failures(summary)),
            vec![("", "2 failed, 300 passed in 0.14s")]
        );
        // The same counts in another run are the same failure; other counts are not.
        assert_eq!(
            signature_of(None, "2 failed, 300 passed in 0.14s"),
            signature_of(None, "2 failed, 300 passed in 3.91s")
        );
        let grep_n = include_str!("../../../../fixtures/outputs/go_test_grep_n.txt");
        let found = extract_failures(grep_n);
        let tests: Vec<&str> = found.iter().filter_map(|f| f.test.as_deref()).collect();
        assert_eq!(
            tests,
            vec!["TestTotalWithTax", "TestFind/id=42", "TestPanics"]
        );
        // A bare exception after a traceback (`raise AssertionError`).
        let bare = "Traceback (most recent call last):\n  File \"t.py\", line 3\nAssertionError\n";
        assert_eq!(pairs(&extract_failures(bare)), vec![("", "AssertionError")]);
        // A passing summary is not a failure.
        assert!(extract_failures("300 passed in 0.14s").is_empty());
        assert!(extract_failures("test result: ok. 2 passed; 0 failed; 0 ignored").is_empty());
    }

    /// Cargo's summary lines are not failures: two different compile errors stay two.
    #[test]
    fn cargo_summary_lines_are_not_failures() {
        let a = "error[E0425]: cannot find value `x` in this scope\nerror: could not compile `app` (bin \"app\") due to 1 previous error\n";
        let b = "error[E0308]: mismatched types\nerror: could not compile `app` (bin \"app\") due to 1 previous error\n";
        let (fa, fb) = (extract_failures(a), extract_failures(b));
        assert_eq!(fa.len(), 1, "{fa:?}");
        assert_ne!(fa[0].signature, fb[0].signature);
        assert!(extract_failures("error: aborting due to 2 previous errors\n").is_empty());
    }

    /// I3, L1: a passing run that logs an error is not a failure.
    #[test]
    fn errors_in_passing_runs_are_not_failures() {
        let pytest = "tests/test_retry.py ConnectionError: refused\n...\n3 passed in 0.12s\n";
        assert!(extract_failures(pytest).is_empty());
        let cargo = "ConfigError: missing key\ntest result: ok. 4 passed; 0 failed\n";
        assert!(extract_failures(cargo).is_empty());
        let failing = "ConnectionError: refused\n1 failed, 2 passed in 0.12s\n";
        assert!(!extract_failures(failing).is_empty());
    }

    /// I3, L6: unittest, spaced pytest ids, jest warnings.
    #[test]
    fn unittest_and_odd_ids() {
        let ut = "======================================================================\nFAIL: test_total (tests.test_cart.CartTest.test_total)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File \"tests/test_cart.py\", line 5, in test_total\n    self.assertEqual(total(), 6)\nAssertionError: 5 != 6\n\n----------------------------------------------------------------------\nRan 1 test in 0.001s\n\nFAILED (failures=1)\n";
        assert_eq!(
            pairs(&extract_failures(ut)),
            [(
                "tests.test_cart.CartTest.test_total",
                "AssertionError: 5 != 6"
            )]
        );
        let spaced = "FAILED tests/t.py::test_x[a b] - assert 1 == 2\n";
        assert_eq!(
            pairs(&extract_failures(spaced)),
            [("tests/t.py::test_x[a b]", "assert 1 == 2")]
        );
        let jest =
            "  ● Validation Warning:\n\n  Unknown option \"x\"\n\nTests:       3 passed, 3 total\n";
        assert!(extract_failures(jest).is_empty());
    }

    /// I3, L5: values tell failures apart; locations and timings do not.
    #[test]
    fn normalizing_keeps_values() {
        let differ = [
            ("assert (3, 2) == (3, 1)", "assert (4, 0) == (3, 1)"),
            ("got '/api/v1/orders'", "got '/api/v3/items'"),
            ("ratio 1/3", "ratio 2/7"),
            (r#"{"qty":1}"#, r#"{"qty":7}"#),
            ("timeout '5s'", "timeout '7s'"),
            ("name tmpl", "name tmp_other"),
        ];
        for (a, b) in differ {
            assert_ne!(normalize_message(a), normalize_message(b), "{a} | {b}");
        }
        let same = [
            (
                "/home/a/x/tests/test_a.py:12: KeyError",
                "/tmp/ci/y/tests/test_a.py:40: KeyError",
            ),
            (
                "could not compile `x` due to 3 previous errors",
                "could not compile `x` due to 2 previous errors",
            ),
            ("<Mock id='139872334'>", "<Mock id='140112'>"),
            ("took 0.12s", "took 3.40s"),
            ("in 340ms", "in 12ms"),
            (
                "/tmp/pytest-of-u/pytest-12/test_a0/out",
                "/tmp/pytest-of-u/pytest-13/test_a0/out",
            ),
            ("Foo.cs(12,5): error", "Foo.cs(40,1): error"),
        ];
        for (a, b) in same {
            assert_eq!(normalize_message(a), normalize_message(b), "{a} | {b}");
        }
    }

    /// Checkpoint C2: Rust or Python source in a file view is not a failure.
    #[test]
    fn source_lines_are_not_failures() {
        let src = "    assert!(out.contains(\"x\"));\n    assert_eq!(a, b);\nassert(true)\n";
        assert!(extract_failures(src).is_empty());
        assert_eq!(extract_failures("assert 1 == 2\n").len(), 1);
    }

    #[test]
    fn go_and_generic() {
        let go = "--- FAIL: TestTotal (0.00s)\n    orders_test.go:12: got 1998, want 2158\nFAIL";
        assert_eq!(extract_failures(go)[0].test.as_deref(), Some("TestTotal"));
        let generic = "Traceback (most recent call last):\n  File \"x.py\", line 3\nTypeError: unsupported operand";
        assert_eq!(
            extract_failures(generic)[0].message,
            "TypeError: unsupported operand"
        );
    }
}
