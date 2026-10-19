//! Ledger records. The ledger is append-only and is the single source of truth.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::broker::{Answer, Source};
use crate::event::{Event, Mode};
use crate::guard::GuardDecision;
use crate::perception::OutputKind;

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub ts_ms: u64,
    /// Sequence numbers of the entries that caused this one. `rein why` follows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<u64>,
    pub body: Body,
}

/// Ledger format version. A ledger without a `format` entry is version 1.
/// Bump it only for a change that older readers would misread, and say so.
pub const LEDGER_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// Always the first entry (seq 0). Keep this shape stable: readers check it first.
    Format {
        version: u32,
    },
    /// An event as received. Large text is replaced with `<blob:HASH>`.
    Event {
        event: Event,
    },
    /// A stored original tool output. `id` is what `rein show` takes.
    Output {
        id: String,
        blob: String,
        output_kind: OutputKind,
        lines: usize,
        tokens: usize,
    },
    Judgment {
        question: String,
        version: u32,
        subject: String,
        answer: Answer,
        source: Source,
    },
    /// `applied` is false in look mode or when the agent lacks the capability.
    /// `unsupported` is true when the agent lacks the capability, so no mode can apply it.
    Action {
        id: String,
        action: ActionKind,
        reason: String,
        applied: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        unsupported: bool,
    },
    Adaptation {
        knob: String,
        from: String,
        to: String,
        reason: String,
    },
    ModeChanged {
        from: Mode,
        to: Mode,
    },
    /// The user allowed, once, the edit that guard action `action` blocked.
    Override {
        action: String,
    },
    /// Config values that differ from the defaults, recorded at session start.
    Config {
        overrides: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionKind {
    Shape {
        output_id: String,
        from_tokens: usize,
        to_tokens: usize,
        from_lines: usize,
        to_lines: usize,
    },
    Steer {
        trigger: SteerTrigger,
        text: String,
    },
    Guard {
        path: String,
        decision: GuardDecision,
    },
    /// A command the agent may not run (it would lift a guard).
    BlockCommand {
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum SteerTrigger {
    /// One steer covers every failure that reached the threshold in the same run.
    Loop {
        signatures: Vec<String>,
        /// First occurrence of the oldest failure to the steer. None in older ledgers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Cycle {
        path: String,
    },
}
