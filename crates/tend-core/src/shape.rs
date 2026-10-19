//! Shaping: reduce long tool output to a short, faithful summary.
//!
//! Faithful means: every failure line stays (up to a per-block limit), passing
//! lines become counts, repeated lines collapse, the middle of plain noise goes.
//! The full original stays retrievable with `tend show <id>`.

use std::collections::HashMap;

use crate::text::{clean_lines, estimate_tokens};

/// Longest block (in lines) that repeat collapsing looks for.
const MAX_REPEAT_BLOCK: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shaped {
    pub text: String,
    pub from_lines: usize,
    pub to_lines: usize,
    pub from_tokens: usize,
    pub to_tokens: usize,
}

#[derive(Debug, Clone, Copy)]
struct Params {
    head: usize,
    tail: usize,
    block_max: usize,
}

/// Level 0 = off. Higher level = tighter summary.
fn params(level: u8) -> Option<Params> {
    match level {
        0 => None,
        1 => Some(Params {
            head: 40,
            tail: 40,
            block_max: 60,
        }),
        2 => Some(Params {
            head: 20,
            tail: 20,
            block_max: 30,
        }),
        _ => Some(Params {
            head: 8,
            tail: 8,
            block_max: 12,
        }),
    }
}

fn is_pass(l: &str) -> bool {
    re!(r"(\bPASSED\b|\.\.\. ok$|^\s*(✓|√|✔)\s|^ok\s+\S|--- PASS|^\s*PASS\s)").is_match(l)
}

/// Claude Code's mark where it cut long output: `... [16523 characters truncated] ...`.
fn is_cut_marker(l: &str) -> bool {
    re!(r"^\.\.\. \[\d+ characters truncated\] \.\.\.$").is_match(l.trim())
}

fn is_important(l: &str) -> bool {
    re!(r"(?i)(error|fail|panic|assert|exception|traceback|expected|✕|×|^\s*●\s|^\s*E\s{2,}|^\s{2,}[-+]\s{2,}\S)").is_match(l)
}

/// Return a summary, or `None` when the summary would not save enough
/// (`to_tokens > from_tokens * max_ratio`).
pub fn shape(output: &str, level: u8, max_ratio: f32) -> Option<Shaped> {
    let p = params(level)?;
    let lines = clean_lines(output);
    let n = lines.len();
    let mut keep = vec![false; n];

    // Head and tail give context (command banner, final summary). Passing lines never count.
    // A truncation marker from the agent always stays: the output was already incomplete.
    for (i, k) in keep.iter_mut().enumerate() {
        if ((i < p.head || i + p.tail >= n) && !is_pass(&lines[i])) || is_cut_marker(&lines[i]) {
            *k = true;
        }
    }

    // Important lines start a block that runs to the next blank line (max block_max lines).
    let mut i = 0;
    while i < n {
        if is_important(&lines[i]) && !is_pass(&lines[i]) {
            let end = (i + p.block_max).min(n);
            let mut j = i;
            while j < end && (j == i || !lines[j].trim().is_empty()) {
                keep[j] = true;
                j += 1;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }

    keep_go_logs(&lines, &mut keep, p.block_max);

    // Repeated blocks: keep the first copy of an important block, drop the copies.
    let repeats = find_repeats(&lines);
    let mut copies: HashMap<usize, &Repeat> = HashMap::new();
    for r in &repeats {
        let first = r.start..r.start + r.len;
        if r.len <= p.block_max && first.clone().any(|i| is_important(&lines[i])) {
            first.for_each(|i| keep[i] = true);
        }
        copies.insert(r.start + r.len, r);
    }

    let mut out: Vec<String> = Vec::new();
    let mut gap = Gap::default();
    let mut idx = 0;
    while idx < n {
        if let Some(r) = copies.get(&idx) {
            gap.flush(&mut out);
            out.push(format!(
                "[… previous {} lines repeated {} more times …]",
                r.len,
                r.count - 1
            ));
            idx += r.len * (r.count - 1);
        } else if keep[idx] {
            gap.flush(&mut out);
            let mut k = idx + 1;
            while k < n && keep[k] && lines[k] == lines[idx] {
                k += 1;
            }
            let reps = k - idx;
            if reps > 1 {
                out.push(format!("{}  [×{reps}]", lines[idx]));
            } else {
                out.push(lines[idx].clone());
            }
            idx = k;
        } else {
            gap.add(&lines[idx]);
            idx += 1;
        }
    }
    gap.flush(&mut out);

    let text = out.join("\n");
    let from_tokens = estimate_tokens(output);
    let to_tokens = estimate_tokens(&text);
    if to_tokens as f32 > from_tokens as f32 * max_ratio {
        return None;
    }
    Some(Shaped {
        to_lines: out.len(),
        text,
        from_lines: n,
        from_tokens,
        to_tokens,
    })
}

/// `count` consecutive copies of the `len`-line block that starts at `start`.
#[derive(Debug, PartialEq, Eq)]
struct Repeat {
    start: usize,
    len: usize,
    count: usize,
}

/// Find blocks of 2+ lines repeated back to back (identical text). The shortest period wins.
/// Single repeated lines are left to the line-level `[×N]` collapse.
fn find_repeats(lines: &[String]) -> Vec<Repeat> {
    // Compare line ids, not strings. Ids only feed comparisons, never the output.
    let mut ids_of: HashMap<&str, usize> = HashMap::new();
    let ids: Vec<usize> = lines
        .iter()
        .map(|l| {
            let next = ids_of.len();
            *ids_of.entry(l.as_str()).or_insert(next)
        })
        .collect();
    let n = ids.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let mut found = None;
        for len in 2..=MAX_REPEAT_BLOCK.min((n - i) / 2) {
            let block = &ids[i..i + len];
            // A block of one repeated line is a line repeat, not a block repeat.
            if block.iter().all(|x| *x == block[0]) {
                continue;
            }
            let mut count = 1;
            while i + (count + 1) * len <= n
                && ids[i + count * len..i + (count + 1) * len] == *block
            {
                count += 1;
            }
            if count > 1 {
                found = Some(Repeat {
                    start: i,
                    len,
                    count,
                });
                break;
            }
        }
        match found {
            Some(r) => {
                i += r.len * r.count;
                out.push(r);
            }
            None => i += 1,
        }
    }
    out
}

/// `go test -v` prints a test's logs (its failure message) before `--- FAIL: name`,
/// under `=== RUN   name`. Keep that header and its logs for every kept failure.
fn keep_go_logs(lines: &[String], keep: &mut [bool], block_max: usize) {
    let is_marker = |l: &str| {
        let t = l.trim_start();
        t.starts_with("=== ") || t.starts_with("--- ")
    };
    for i in 0..lines.len() {
        if !keep[i] {
            continue;
        }
        let Some(c) = re!(r"^\s*--- FAIL: (\S+)").captures(&lines[i]) else {
            continue;
        };
        let run = format!("=== RUN   {}", &c[1]);
        let Some(r) = lines[..i].iter().rposition(|l| l.trim() == run) else {
            continue;
        };
        keep[r] = true;
        for j in (r + 1..i).take(block_max) {
            if is_marker(&lines[j]) {
                break;
            }
            keep[j] = true;
        }
    }
}

/// A run of dropped lines between kept lines.
#[derive(Default)]
struct Gap {
    dropped: usize,
    pass: usize,
    blank: usize,
}

impl Gap {
    fn add(&mut self, line: &str) {
        self.dropped += 1;
        if is_pass(line) {
            self.pass += 1;
        }
        if line.trim().is_empty() {
            self.blank += 1;
        }
    }

    /// Write the omission marker. A gap of blank lines only becomes one blank line.
    fn flush(&mut self, out: &mut Vec<String>) {
        if self.dropped == 0 {
            return;
        }
        if self.blank == self.dropped {
            out.push(String::new());
        } else if self.pass > 0 {
            out.push(format!(
                "[… {} lines omitted ({} passing) …]",
                self.dropped, self.pass
            ));
        } else {
            out.push(format!("[… {} lines omitted …]", self.dropped));
        }
        *self = Gap::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perception::extract_failures;

    const PYTEST: &str = include_str!("../../../fixtures/outputs/pytest_fail.txt");
    const CARGO: &str = include_str!("../../../fixtures/outputs/cargo_fail.txt");

    #[test]
    fn keeps_every_failure_and_drops_passing_lines() {
        for (name, raw) in [("pytest", PYTEST), ("cargo", CARGO)] {
            let s = shape(raw, 2, 0.7).unwrap_or_else(|| panic!("{name}: no summary"));
            assert!(s.to_tokens * 3 < s.from_tokens, "{name}: not small enough");
            for f in extract_failures(raw) {
                assert_eq!(
                    extract_failures(&s.text)
                        .iter()
                        .filter(|g| g.signature == f.signature)
                        .count(),
                    1,
                    "{name}: lost failure {}",
                    f.summary()
                );
            }
            assert!(s.text.contains("passing"), "{name}: no passing count");
        }
    }

    #[test]
    fn keeps_every_failure_for_go_jest_vitest() {
        let fixtures = [
            (
                "go",
                include_str!("../../../fixtures/outputs/go_test_fail.txt"),
            ),
            (
                "go -v",
                include_str!("../../../fixtures/outputs/go_test_v_fail.txt"),
            ),
            (
                "jest",
                include_str!("../../../fixtures/outputs/jest_fail.txt"),
            ),
            (
                "vitest",
                include_str!("../../../fixtures/outputs/vitest_fail.txt"),
            ),
        ];
        for (name, raw) in fixtures {
            let want = extract_failures(raw);
            assert_eq!(want.len(), 3, "{name}: fixture has 3 failures");
            for level in 1..=3 {
                let Some(s) = shape(raw, level, 1.0) else {
                    continue;
                };
                let got = extract_failures(&s.text);
                for f in &want {
                    assert!(
                        got.iter().any(|g| g.signature == f.signature),
                        "{name} level {level}: lost {}\n{}",
                        f.summary(),
                        s.text
                    );
                }
            }
        }
    }

    #[test]
    fn repeated_traces_collapse_to_one_with_a_count() {
        let raw = include_str!("../../../fixtures/outputs/repeated_traces.txt");
        for level in 1..=3 {
            let s = shape(raw, level, 0.7).expect("summary");
            assert_eq!(
                s.text.matches("Traceback (most recent call last):").count(),
                1
            );
            assert_eq!(
                s.text
                    .matches("ZeroDivisionError: division by zero")
                    .count(),
                1
            );
            assert!(
                s.text
                    .contains("[… previous 10 lines repeated 49 more times …]"),
                "level {level}:\n{}",
                s.text
            );
            assert!(s.text.contains("worker: batch done"), "tail kept");
            assert_eq!(extract_failures(&s.text), extract_failures(raw));
        }
    }

    #[test]
    fn different_blocks_do_not_collapse() {
        let raw = (0..30)
            .map(|i| format!("Traceback\nValueError: bad value {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = shape(&raw, 2, 1.0).expect("summary");
        assert!(!s.text.contains("repeated"), "{}", s.text);
    }

    /// Claude Code cut this output in the middle. The summary must say so.
    #[test]
    fn keeps_claude_code_truncation_marker() {
        let raw = include_str!("../../../fixtures/outputs/pytest_cut_by_claude.txt");
        let s = shape(raw, 3, 1.0).expect("summary");
        assert!(
            s.text.contains("... [16523 characters truncated] ..."),
            "{}",
            s.text
        );
    }

    #[test]
    fn short_output_is_not_shaped() {
        assert!(shape("ok\nall good\n", 2, 0.7).is_none());
    }

    #[test]
    fn level_zero_is_off() {
        assert!(shape(PYTEST, 0, 0.7).is_none());
    }

    #[test]
    fn tighter_level_is_smaller() {
        let l1 = shape(PYTEST, 1, 1.0).unwrap();
        let l3 = shape(PYTEST, 3, 1.0).unwrap();
        assert!(l3.to_tokens <= l1.to_tokens);
    }
}
