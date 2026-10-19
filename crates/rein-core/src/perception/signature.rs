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
    let s = re!(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
        .replace_all(&s, "<ts>");
    let s = re!(r"\b\d+(\.\d+)?\s?(ms|s|sec|seconds)\b").replace_all(&s, "<dur>");
    let s = re!(r"(?:[A-Za-z]:)?(?:[\w.~-]*[/\\])+[\w.-]+").replace_all(&s, "<path>");
    let s = re!(r":\d+(:\d+)?\b").replace_all(&s, ":<n>");
    let s = re!(r"\(\d+,\s*\d+\)").replace_all(&s, "(<n>)");
    let s = re!(r"\bline \d+").replace_all(&s, "line <n>");
    let s = re!(r"\btmp[\w-]*").replace_all(&s, "<tmp>");
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
    let lines = clean_lines(output);
    let panics = cargo_panics(&lines);
    let mut found: Vec<(Option<String>, String)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        // pytest short summary: FAILED tests/test_api.py::test_get - AssertionError: ...
        if let Some(c) = re!(r"^(?:FAILED|ERROR) (\S+)(?: - (.*))?$").captures(line) {
            let msg = c.get(2).map_or("", |m| m.as_str()).trim().to_string();
            found.push((Some(c[1].to_string()), msg));
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
            found.push((Some(c[1].trim().to_string()), jest_message(&lines, i)));
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

    if found.is_empty() {
        for line in &lines {
            let l = line.trim();
            if re!(r"^\w+(Error|Exception): .+|^error(\[E\d+\])?: .+|\berror TS\d+: .+|:\d+:\d+: error: .+")
                .is_match(l)
            {
                found.push((None, l.to_string()));
            }
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
            });
            if out.len() >= MAX_FAILURES {
                break;
            }
        }
    }
    out
}

/// Map cargo test thread names to panic messages. Handles both formats:
/// `panicked at 'msg', src/x.rs:1:2` and `panicked at src/x.rs:1:2:` + message on the next line.
fn cargo_panics(lines: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(c) = re!(r"^thread '([^']+)' panicked at (.*)$").captures(l) {
            let rest = c[2].trim();
            let msg = if rest.ends_with(':') {
                next_nonempty(lines, i).unwrap_or_default()
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
        let f = extract_failures(CARGO);
        assert_eq!(f.len(), 2);
        assert_eq!(
            f[0].test.as_deref(),
            Some("orders::tests::total_includes_tax")
        );
        assert_eq!(f[0].message, "assertion `left == right` failed");
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
