//! Loop detection.
//!
//! A loop is one of these:
//! - The same failure signature after `threshold` or more fix attempts. An attempt is
//!   one or more edits followed by a run that still shows the failure. Edits made
//!   between two runs count once, so a long stretch of unrelated work is one attempt.
//! - A file returns to an earlier content hash (edit, revert, edit).
//!
//! Failures at the start of a session are normal: they count only after edits.

use std::collections::{HashMap, HashSet};

use crate::perception::Failure;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopSignal {
    Loop {
        signature: String,
        summary: String,
        edits: u64,
        /// When this failure was first seen (ms on the caller's clock).
        first_ms: u64,
    },
    Cycle {
        path: String,
    },
}

/// What the detector saw for one failure in one run.
#[derive(Debug, Clone)]
pub struct Observation {
    pub failure: Failure,
    /// Message of the earlier occurrence, if this signature was seen before.
    pub previous_message: Option<String>,
    pub signal: Option<LoopSignal>,
}

#[derive(Debug, Clone)]
struct SigState {
    /// Edit count when this failure was last seen.
    last_edit: u64,
    /// Runs that still failed after at least one edit.
    attempts: u64,
    first_ms: u64,
    steered: bool,
    command: String,
    /// The failure as first seen.
    failure: Failure,
}

#[derive(Debug, Clone)]
pub struct LoopDetector {
    threshold: u64,
    edits: u64,
    sigs: HashMap<String, SigState>,
    files: HashMap<String, Vec<String>>,
    cycled: HashSet<String>,
}

impl LoopDetector {
    pub fn new(threshold: u64) -> Self {
        Self {
            threshold,
            edits: 0,
            sigs: HashMap::new(),
            files: HashMap::new(),
            cycled: HashSet::new(),
        }
    }

    /// Number of edits that changed file content.
    pub fn edits(&self) -> u64 {
        self.edits
    }

    /// An edit with no content hash (a Bash write). It counts toward fix attempts but
    /// cannot show an edit cycle.
    pub fn note_edit(&mut self) {
        self.edits += 1;
    }

    pub fn on_edit(&mut self, path: &str, hash: &str) -> Option<LoopSignal> {
        let hist = self.files.entry(path.to_string()).or_default();
        if hist.last().map(String::as_str) == Some(hash) {
            return None; // no content change
        }
        self.edits += 1;
        let repeated = hist.iter().any(|h| h == hash);
        hist.push(hash.to_string());
        if repeated && self.cycled.insert(path.to_string()) {
            return Some(LoopSignal::Cycle {
                path: path.to_string(),
            });
        }
        None
    }

    /// Call after each command run with the failures found in its output.
    /// A signature that this command no longer produces is forgotten, except after a run
    /// that shows only the runner's summary line (`| tail -1`): it failed without saying
    /// which tests, so this command's known failures stay, and the run is an attempt for
    /// each of them (I3, L4).
    pub fn on_run(&mut self, command: &str, failures: &[Failure], now_ms: u64) -> Vec<Observation> {
        let current: HashSet<&str> = failures.iter().map(|f| f.signature.as_str()).collect();
        let partial = !failures.is_empty() && failures.iter().all(|f| f.summary_only);
        let mut seen: Vec<Failure> = Vec::new();
        if partial {
            let mut carried: Vec<&SigState> = self
                .sigs
                .iter()
                .filter(|(sig, st)| {
                    st.command == command
                        && !st.failure.summary_only
                        && !current.contains(sig.as_str())
                })
                .map(|(_, st)| st)
                .collect();
            carried.sort_by(|a, b| {
                (a.first_ms, &a.failure.signature).cmp(&(b.first_ms, &b.failure.signature))
            });
            seen.extend(carried.into_iter().map(|st| st.failure.clone()));
        } else {
            self.sigs
                .retain(|sig, st| st.command != command || current.contains(sig.as_str()));
        }
        seen.extend(failures.iter().cloned());
        seen.iter()
            .map(|f| self.observe(command, f, now_ms))
            .collect()
    }

    fn observe(&mut self, command: &str, f: &Failure, now_ms: u64) -> Observation {
        let edits_now = self.edits;
        match self.sigs.get_mut(&f.signature) {
            Some(st) => {
                if edits_now > st.last_edit {
                    st.attempts += 1;
                    st.last_edit = edits_now;
                }
                let edits = st.attempts;
                let previous_message = Some(st.failure.message.clone());
                let signal = if edits >= self.threshold && !st.steered {
                    st.steered = true;
                    Some(LoopSignal::Loop {
                        signature: f.signature.clone(),
                        summary: f.summary(),
                        edits,
                        first_ms: st.first_ms,
                    })
                } else {
                    None
                };
                Observation {
                    failure: f.clone(),
                    previous_message,
                    signal,
                }
            }
            None => {
                self.sigs.insert(
                    f.signature.clone(),
                    SigState {
                        last_edit: edits_now,
                        attempts: 0,
                        first_ms: now_ms,
                        steered: false,
                        command: command.to_string(),
                        failure: f.clone(),
                    },
                );
                Observation {
                    failure: f.clone(),
                    previous_message: None,
                    signal: None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perception::signature_of;

    #[test]
    fn carried_failures_order_is_deterministic() {
        for _ in 0..20 {
            let mut d = LoopDetector::new(3);
            let fs: Vec<Failure> = (0..8).map(|i| fail(&format!("m{i}"))).collect();
            d.on_run("pytest", &fs, 5);
            let out = d.on_run("pytest", &[summary("1 failed")], 6);
            let mut want: Vec<String> = fs.iter().map(|f| f.signature.clone()).collect();
            want.sort();
            let got: Vec<String> = out
                .iter()
                .take(8)
                .map(|o| o.failure.signature.clone())
                .collect();
            assert_eq!(got, want);
        }
    }

    fn fail(msg: &str) -> Failure {
        Failure {
            signature: signature_of(Some("t"), msg),
            test: Some("t".into()),
            message: msg.into(),
            summary_only: false,
        }
    }

    fn summary(msg: &str) -> Failure {
        Failure {
            signature: signature_of(None, msg),
            test: None,
            message: msg.into(),
            summary_only: true,
        }
    }

    /// L4: `| tail -3` (the failing test) and `| tail -1` (only the summary) alternate;
    /// the loop is still found.
    #[test]
    fn summary_only_runs_keep_the_loop() {
        let mut d = LoopDetector::new(3);
        let full = [fail("assert 500 == 200")];
        let short = [summary("1 failed, 2 passed in 0.12s")];
        d.on_run("pytest", &full, 0);
        let mut steered = false;
        for i in 1..=3u64 {
            d.on_edit("src/app.py", &format!("h{i}"));
            let f: &[Failure] = if i % 2 == 1 { &short } else { &full };
            steered |= d
                .on_run("pytest", f, i)
                .iter()
                .any(|o| matches!(&o.signal, Some(LoopSignal::Loop { signature, .. }) if *signature == full[0].signature));
        }
        assert!(steered);
    }

    #[test]
    fn loop_after_three_distinct_edits_only_once() {
        let mut d = LoopDetector::new(3);
        let f = [fail("assert 500 == 200")];
        assert!(d.on_run("pytest", &f, 1_000)[0].signal.is_none());
        for (i, h) in ["h1", "h2", "h3"].iter().enumerate() {
            d.on_edit("src/app.py", h);
            let obs = d.on_run("pytest", &f, 2_000 + i as u64);
            assert_eq!(obs[0].signal.is_some(), i == 2, "edit {i}");
            if let Some(LoopSignal::Loop { first_ms, .. }) = &obs[0].signal {
                assert_eq!(*first_ms, 1_000, "first occurrence, not the latest");
            }
        }
        d.on_edit("src/app.py", "h4");
        assert!(
            d.on_run("pytest", &f, 9_000)[0].signal.is_none(),
            "steer only once"
        );
    }

    /// Many edits between two runs are one attempt, not many.
    #[test]
    fn a_batch_of_edits_is_one_attempt() {
        let mut d = LoopDetector::new(3);
        let f = [fail("assert 500 == 200")];
        d.on_run("pytest", &f, 0);
        for i in 0..50 {
            d.on_edit("src/other.py", &format!("h{i}"));
        }
        assert!(d.on_run("pytest", &f, 1)[0].signal.is_none(), "1 attempt");
        d.on_edit("src/app.py", "a");
        assert!(d.on_run("pytest", &f, 2)[0].signal.is_none(), "2 attempts");
        assert!(
            d.on_run("pytest", &f, 3)[0].signal.is_none(),
            "a re-run without edit"
        );
        d.on_edit("src/app.py", "b");
        assert!(d.on_run("pytest", &f, 4)[0].signal.is_some(), "3 attempts");
    }

    #[test]
    fn same_hash_is_not_an_edit() {
        let mut d = LoopDetector::new(3);
        d.on_edit("a", "h1");
        d.on_edit("a", "h1");
        assert_eq!(d.edits(), 1);
    }

    #[test]
    fn fixed_failure_is_forgotten() {
        let mut d = LoopDetector::new(1);
        let f = [fail("boom")];
        d.on_run("pytest", &f, 0);
        d.on_run("pytest", &[], 0);
        d.on_edit("a", "h1");
        assert!(d.on_run("pytest", &f, 0)[0].previous_message.is_none());
    }

    #[test]
    fn revert_is_a_cycle() {
        let mut d = LoopDetector::new(3);
        assert!(d.on_edit("a", "h1").is_none());
        assert!(d.on_edit("a", "h2").is_none());
        assert_eq!(
            d.on_edit("a", "h1"),
            Some(LoopSignal::Cycle { path: "a".into() })
        );
    }
}
