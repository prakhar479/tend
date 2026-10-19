//! Judgment broker and question catalog.
//!
//! A question is a typed, versioned judgment. v1 answers every question with rules.
//! Deciders (external judges) plug in through the `Decider` trait. They judge; they never act.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::guard;
use crate::perception::signature_of;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum Answer {
    Same,
    Different,
    Weakened { reasons: Vec<String> },
    Neutral,
    Strengthened,
    Unsure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Rule,
    Decider { name: String },
    Fallback,
}

#[derive(Debug, Clone)]
pub enum Question<'a> {
    /// Are these two failure messages the same underlying failure?
    SameFailure {
        test: Option<&'a str>,
        a: &'a str,
        b: &'a str,
    },
    /// Does this edit to a test file weaken the tests?
    TestWeakened {
        path: &'a str,
        old: &'a str,
        new: &'a str,
    },
}

impl Question<'_> {
    pub fn name(&self) -> &'static str {
        match self {
            Question::SameFailure { .. } => "same_failure",
            Question::TestWeakened { .. } => "test_weakened",
        }
    }

    /// Bump when the question's meaning or answer type changes.
    pub fn version(&self) -> u32 {
        1
    }
}

/// External judge. Implementations live outside tend-core because they do I/O.
pub trait Decider: Send {
    fn name(&self) -> &str;
    fn handles(&self, question: &str) -> bool;
    /// Return `None` when the deadline passes or the answer is invalid.
    fn answer(&self, question: &Question<'_>, deadline: Duration) -> Option<Answer>;
}

#[derive(Default)]
pub struct Broker {
    deciders: Vec<Box<dyn Decider>>,
}

impl Broker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, decider: Box<dyn Decider>) {
        self.deciders.push(decider);
    }

    pub fn decider_names(&self) -> Vec<&str> {
        self.deciders.iter().map(|d| d.name()).collect()
    }

    /// Fast path. Returns the rule answer; never waits for a decider.
    // TODO(Phase 4): run enabled deciders off the fast path with a deadline,
    // validate their answers, and record late answers for the session model.
    pub fn ask(&self, q: &Question<'_>) -> (Answer, Source) {
        (rule_answer(q), Source::Rule)
    }
}

pub fn rule_answer(q: &Question<'_>) -> Answer {
    match q {
        Question::SameFailure { test, a, b } => {
            if signature_of(*test, a) == signature_of(*test, b) {
                Answer::Same
            } else {
                Answer::Different
            }
        }
        Question::TestWeakened { old, new, .. } => guard::assess(old, new),
    }
}
