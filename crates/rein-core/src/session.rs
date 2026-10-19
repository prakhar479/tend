//! Session model: the small, readable state that decisions depend on.
//! Scope is one session. Nothing carries over.

use std::collections::BTreeMap;

use crate::event::Mode;
use crate::guard::GuardDecision;
use crate::perception::OutputKind;

pub const MAX_LEVEL: u8 = 3;
pub const DEFAULT_LEVEL: u8 = 2;

/// One knob change, with a one-sentence reason for the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub knob: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct SessionModel {
    pub mode: Mode,
    /// Wanted test-guard strictness. The controller downgrades it per capabilities.
    pub guard: GuardDecision,
    levels: BTreeMap<OutputKind, u8>,
    unreopened: BTreeMap<OutputKind, u32>,
    tighten_after: u32,
}

impl SessionModel {
    pub fn new(mode: Mode, tighten_after: u32) -> Self {
        Self {
            mode,
            guard: GuardDecision::Ask,
            levels: OutputKind::ALL
                .iter()
                .map(|k| (*k, DEFAULT_LEVEL))
                .collect(),
            unreopened: BTreeMap::new(),
            tighten_after,
        }
    }

    pub fn shape_level(&self, kind: OutputKind) -> u8 {
        self.levels.get(&kind).copied().unwrap_or(DEFAULT_LEVEL)
    }

    pub fn levels(&self) -> &BTreeMap<OutputKind, u8> {
        &self.levels
    }

    /// Call after a summary reached the agent. Tightens after `tighten_after`
    /// summaries of a kind with no re-open.
    pub fn on_shaped(&mut self, kind: OutputKind) -> Option<Change> {
        let count = {
            let n = self.unreopened.entry(kind).or_insert(0);
            *n += 1;
            *n
        };
        if count < self.tighten_after {
            return None;
        }
        self.unreopened.insert(kind, 0);
        let from = self.shape_level(kind);
        if from == 0 || from >= MAX_LEVEL {
            return None;
        }
        let to = from + 1;
        self.levels.insert(kind, to);
        Some(Change {
            knob: format!("shape_level.{}", kind.label()),
            from: from.to_string(),
            to: to.to_string(),
            reason: format!(
                "Tightened {} summaries: the agent did not re-open the last {} summaries.",
                kind.label(),
                self.tighten_after
            ),
        })
    }

    /// Call when the agent re-opens a summary. Loosens that kind one level.
    /// `what` completes "the agent …", for example "re-opened o-4".
    pub fn on_reopen(&mut self, kind: OutputKind, what: &str) -> Option<Change> {
        self.unreopened.insert(kind, 0);
        let from = self.shape_level(kind);
        if from == 0 {
            return None;
        }
        let to = from - 1;
        self.levels.insert(kind, to);
        let verb = if to == 0 { "Stopped" } else { "Loosened" };
        Some(Change {
            knob: format!("shape_level.{}", kind.label()),
            from: from.to_string(),
            to: to.to_string(),
            reason: format!("{verb} {} summaries: the agent {what}.", kind.label()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tightens_after_quiet_summaries() {
        let mut s = SessionModel::new(Mode::Optimize, 3);
        assert!(s.on_shaped(OutputKind::Test).is_none());
        assert!(s.on_shaped(OutputKind::Test).is_none());
        let c = s.on_shaped(OutputKind::Test).unwrap();
        assert_eq!((c.from.as_str(), c.to.as_str()), ("2", "3"));
        assert_eq!(s.shape_level(OutputKind::Test), 3);
        assert_eq!(s.shape_level(OutputKind::Build), DEFAULT_LEVEL);
    }

    #[test]
    fn reopen_loosens_and_resets_count() {
        let mut s = SessionModel::new(Mode::Optimize, 2);
        s.on_shaped(OutputKind::Test);
        let c = s.on_reopen(OutputKind::Test, "re-opened o-4").unwrap();
        assert!(c.reason.contains("o-4"));
        assert_eq!(s.shape_level(OutputKind::Test), 1);
        assert!(s.on_shaped(OutputKind::Test).is_none());
    }
}
