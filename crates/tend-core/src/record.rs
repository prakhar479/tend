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
    /// Sequence numbers of the entries that caused this one. `tend why` follows them.
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
    /// A stored original tool output. `id` is what `tend show` takes.
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
    /// The reply carrying these applied actions came after the shim gave up, so the
    /// agent never got them (D9). Readers treat them as not applied.
    Undelivered {
        actions: Vec<String>,
        reason: String,
    },
    /// Config values that differ from the defaults, recorded at session start.
    Config {
        overrides: BTreeMap<String, String>,
    },
}

/// Read actions that an `undelivered` entry names as not applied, with a note in the
/// reason (D9). The ledger itself is never changed.
pub fn settle_undelivered(entries: &mut [Entry]) {
    let late: std::collections::BTreeSet<String> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::Undelivered { actions, .. } => Some(actions.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    if late.is_empty() {
        return;
    }
    for e in entries.iter_mut() {
        if let Body::Action {
            id,
            applied,
            reason,
            ..
        } = &mut e.body
        {
            if *applied && late.contains(id) {
                *applied = false;
                reason.push_str(" [not delivered: the reply came too late]");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undelivered_actions_read_as_not_applied() {
        let action = |id: &str| Entry {
            seq: 0,
            ts_ms: 0,
            causes: vec![],
            body: Body::Action {
                id: id.into(),
                action: ActionKind::Wrap {
                    tool_use_id: "t".into(),
                },
                reason: "Wrapped.".into(),
                applied: true,
                unsupported: false,
            },
        };
        let late = Entry {
            seq: 2,
            ts_ms: 0,
            causes: vec![0],
            body: Body::Undelivered {
                actions: vec!["a-1".into()],
                reason: "late".into(),
            },
        };
        let mut entries = vec![action("a-1"), action("a-2"), late];
        settle_undelivered(&mut entries);
        assert!(
            matches!(&entries[0].body, Body::Action { applied: false, reason, .. } if reason.ends_with("too late]"))
        );
        assert!(matches!(
            &entries[1].body,
            Body::Action { applied: true, .. }
        ));
    }
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
    /// A Bash call run through `tend run`, so its output can be summarized even on failure.
    Wrap {
        tool_use_id: String,
    },
    /// Tell the agent it may retry a call that auto mode denied because tend had
    /// wrapped it in `tend run` (R3.7). The retry runs unwrapped.
    Retry {
        tool_use_id: String,
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
        /// First occurrence of the oldest failure to the steer, idle gaps capped
        /// (`idle_gap_minutes`). None in older ledgers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Cycle {
        path: String,
    },
}
