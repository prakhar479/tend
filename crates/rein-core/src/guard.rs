//! Test guard: rules that detect edits which weaken tests.
//! Rules are language-agnostic and cheap. They give false positives; keep the guard advisory.

use serde::{Deserialize, Serialize};

use crate::broker::Answer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardDecision {
    Warn,
    Ask,
    Deny,
}

pub fn is_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    re!(r"(^|/)(tests?|__tests__|spec|specs)/|(^|/)test_[^/]*\.py$|_test\.(py|go|rs)$|\.(test|spec)\.[cm]?[jt]sx?$|(^|/)conftest\.py$")
        .is_match(&p)
}

fn count(re: &regex::Regex, s: &str) -> usize {
    re.find_iter(s).count()
}

/// Compare the old and new text of an edit to a test file.
pub fn assess(old: &str, new: &str) -> Answer {
    let assert_re = re!(
        r"\bassert\w*\b|\bexpect\s*\(|\.should\b|\bt\.(Error|Errorf|Fatal|Fatalf)\(|\brequire\.\w+\("
    );
    let skip_re = re!(
        r"@pytest\.mark\.(skip|skipif|xfail)\b|\bpytest\.(skip|xfail)\(|\b(it|test|describe)\.(skip|only|todo)\(|\bx(it|describe|test)\(|#\[ignore\]|\bt\.Skip(Now|f)?\(|@(Disabled|Ignore)\b|@unittest\.skip"
    );
    let true_re = re!(r"\|\|\s*true\b");

    let mut reasons = Vec::new();
    let (a_old, a_new) = (count(assert_re, old), count(assert_re, new));
    if a_new < a_old {
        reasons.push(format!("assertions removed ({a_old} → {a_new})"));
    }
    if count(skip_re, new) > count(skip_re, old) {
        reasons.push("skip or focus marker added".to_string());
    }
    if count(true_re, new) > count(true_re, old) {
        reasons.push("`|| true` added".to_string());
    }
    // TODO(Phase 3): expected value changed to the actual value of the last failure.
    // TODO(Phase 3): tolerance increased (approx, rel_tol, toBeCloseTo, delta).

    if !reasons.is_empty() {
        Answer::Weakened { reasons }
    } else if a_new > a_old {
        Answer::Strengthened
    } else {
        Answer::Neutral
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
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["src/app.py", "src/testing.rs", "latest/notes.md"] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn detects_weakening() {
        let old = "    assert r.status == 200\n    assert r.json['id'] == 42\n";
        let new = "    assert r.status in (200, 500)\n";
        assert!(matches!(assess(old, new), Answer::Weakened { .. }));
        assert!(matches!(
            assess("def test_a():\n", "@pytest.mark.skip\ndef test_a():\n"),
            Answer::Weakened { .. }
        ));
        assert!(matches!(
            assess("it('a', f)", "it.only('a', f)"),
            Answer::Weakened { .. }
        ));
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
