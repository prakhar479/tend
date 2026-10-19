//! Controller: the closed loop. Sense → judge → decide → act → record.
//!
//! The controller is the only writer of ledger entries. It returns a `Decision`:
//! entries and blobs to persist, plus a `Response` for the connector to apply.
//! In look mode the response is always empty; actions are recorded with `applied: false`.

use std::collections::{BTreeMap, HashMap};

use crate::broker::{Answer, Broker, Question};
use crate::capability::Capabilities;
use crate::event::{Event, Mode};
use crate::guard::{is_test_path, GuardDecision};
use crate::loops::{LoopDetector, LoopSignal};
use crate::perception::{classify, extract_failures, OutputKind};
use crate::record::{ActionKind, Body, Entry, SteerTrigger, LEDGER_FORMAT};
use crate::session::{Change, SessionModel};
use crate::shape::shape;
use crate::text::{estimate_tokens, sha256_hex};

/// Initial values. Tune them in Phase 2 with real sessions.
#[derive(Debug, Clone)]
pub struct Config {
    /// Shape Bash output only above this size.
    pub shape_threshold_tokens: usize,
    /// Use a summary only if it is at most this fraction of the original.
    pub max_summary_ratio: f32,
    /// Distinct edits before an unchanged failure counts as a loop.
    pub loop_threshold: u64,
    /// Summaries without a re-open before the controller tightens a kind.
    pub tighten_after: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shape_threshold_tokens: 2000,
            max_summary_ratio: 0.7,
            loop_threshold: 3,
            tighten_after: 5,
        }
    }
}

impl Config {
    /// Every knob as `name → value`, in a stable order.
    pub fn fields(&self) -> BTreeMap<String, String> {
        [
            ("loop_threshold", self.loop_threshold.to_string()),
            ("max_summary_ratio", self.max_summary_ratio.to_string()),
            (
                "shape_threshold_tokens",
                self.shape_threshold_tokens.to_string(),
            ),
            ("tighten_after", self.tighten_after.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Knobs that differ from the defaults.
    pub fn overrides(&self) -> BTreeMap<String, String> {
        let defaults = Config::default().fields();
        self.fields()
            .into_iter()
            .filter(|(k, v)| defaults.get(k) != Some(v))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Ask(String),
    Deny(String),
}

/// What the connector must do now. Empty = do nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub replace_output: Option<String>,
    pub add_context: Vec<String>,
    pub permission: Option<Permission>,
}

impl Response {
    pub fn is_empty(&self) -> bool {
        self.replace_output.is_none() && self.add_context.is_empty() && self.permission.is_none()
    }

    pub fn merge(&mut self, other: Response) {
        if other.replace_output.is_some() {
            self.replace_output = other.replace_output;
        }
        self.add_context.extend(other.add_context);
        if self.permission.is_none() {
            self.permission = other.permission;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub hash: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct Decision {
    pub entries: Vec<Entry>,
    pub blobs: Vec<Blob>,
    pub response: Response,
}

pub struct Controller {
    cfg: Config,
    caps: Capabilities,
    session: SessionModel,
    loops: LoopDetector,
    broker: Broker,
    next_seq: u64,
    next_output: u64,
    next_action: u64,
    outputs: HashMap<String, OutputKind>,
}

impl Controller {
    pub fn new(cfg: Config, caps: Capabilities, mode: Mode) -> Self {
        Self {
            session: SessionModel::new(mode, cfg.tighten_after),
            loops: LoopDetector::new(cfg.loop_threshold),
            cfg,
            caps,
            broker: Broker::new(),
            next_seq: 0,
            next_output: 1,
            next_action: 1,
            outputs: HashMap::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.session.mode
    }

    pub fn session(&self) -> &SessionModel {
        &self.session
    }

    pub fn broker_mut(&mut self) -> &mut Broker {
        &mut self.broker
    }

    pub fn set_mode(&mut self, mode: Mode, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        let from = self.session.mode;
        if from != mode {
            self.session.mode = mode;
            self.push(&mut d, now_ms, vec![], Body::ModeChanged { from, to: mode });
        }
        d
    }

    pub fn handle(&mut self, event: Event, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        let is_bash = matches!(&event, Event::ToolCompleted { tool, .. } if tool == "Bash");
        match event {
            _ if is_bash => self.on_bash(&mut d, event, now_ms),
            Event::EditProposed { .. } => self.on_edit_proposed(&mut d, event, now_ms),
            Event::FileEdited { .. } => self.on_file_edited(&mut d, event, now_ms),
            Event::SessionStarted {
                agent,
                cwd,
                transcript_path,
                ..
            } => {
                let event = Event::SessionStarted {
                    agent,
                    cwd,
                    transcript_path,
                    mode: Some(self.session.mode),
                };
                let ev = self.push(&mut d, now_ms, vec![], Body::Event { event });
                let overrides = self.cfg.overrides();
                if !overrides.is_empty() {
                    self.push(&mut d, now_ms, vec![ev], Body::Config { overrides });
                }
            }
            other => {
                self.push(&mut d, now_ms, vec![], Body::Event { event: other });
            }
        }
        d
    }

    fn on_bash(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::ToolCompleted {
            tool_use_id,
            tool,
            input,
            output,
            failed,
        } = event
        else {
            return;
        };
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let info = classify(&command);
        let hash = sha256_hex(&output);
        let stored = Event::ToolCompleted {
            tool_use_id,
            tool,
            input,
            output: format!("<blob:{hash}>"),
            failed,
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });

        // Re-open of a summary: the agent needed the full output.
        if let Some(target) = info.show_target.as_deref() {
            if let Some(kind) = self.outputs.get(target).copied() {
                if let Some(change) = self.session.on_reopen(kind, target) {
                    self.adapt(d, now, ev, change);
                }
            }
            return;
        }

        let output_id = format!("o-{}", self.next_output);
        self.next_output += 1;
        self.outputs.insert(output_id.clone(), info.kind);
        d.blobs.push(Blob {
            hash: hash.clone(),
            content: output.clone(),
        });
        let tokens = estimate_tokens(&output);
        let out = self.push(
            d,
            now,
            vec![ev],
            Body::Output {
                id: output_id.clone(),
                blob: hash,
                output_kind: info.kind,
                lines: output.lines().count(),
                tokens,
            },
        );

        // Loops. All failures that reach the threshold in one run share one steer.
        let failures = extract_failures(&output);
        let mut looped: Vec<(String, String, u64, u64)> = Vec::new();
        for obs in self.loops.on_run(&command, &failures, now) {
            if let Some(prev) = obs.previous_message.as_deref() {
                let q = Question::SameFailure {
                    test: obs.failure.test.as_deref(),
                    a: prev,
                    b: &obs.failure.message,
                };
                let (answer, source) = self.broker.ask(&q);
                let body = Body::Judgment {
                    question: q.name().into(),
                    version: q.version(),
                    subject: obs.failure.signature.clone(),
                    answer,
                    source,
                };
                self.push(d, now, vec![out], body);
            }
            if let Some(LoopSignal::Loop {
                signature,
                summary,
                edits,
                first_ms,
            }) = obs.signal
            {
                looped.push((signature, summary, edits, first_ms));
            }
        }
        if !looped.is_empty() {
            let edits = looped.iter().map(|l| l.2).min().unwrap_or(0);
            let summaries: Vec<&str> = looped.iter().map(|l| l.1.as_str()).collect();
            let signatures: Vec<String> = looped.iter().map(|l| l.0.clone()).collect();
            let first = looped.iter().map(|l| l.3).min().unwrap_or(now);
            let duration_ms = Some(now.saturating_sub(first));
            let text = if looped.len() == 1 {
                format!(
                    "winnow: this failure is unchanged after {edits} different edits: {}. A different approach may be needed.",
                    summaries[0]
                )
            } else {
                format!(
                    "winnow: {} failures are unchanged after {edits} different edits: {}. A different approach may be needed.",
                    looped.len(),
                    summaries.join("; ")
                )
            };
            let reason = format!(
                "Failure signature(s) {} recurred after {edits} distinct edits.",
                signatures.join(", ")
            );
            let trigger = SteerTrigger::Loop {
                signatures,
                duration_ms,
            };
            self.steer(d, now, out, trigger, text, reason);
        }

        // Shaping.
        if tokens < self.cfg.shape_threshold_tokens {
            return;
        }
        let level = self.session.shape_level(info.kind);
        let Some(s) = shape(&output, level, self.cfg.max_summary_ratio) else {
            return;
        };
        let text = format!(
            "{}\n[winnow: {} lines → {} · full output: winnow show {}]",
            s.text, s.from_lines, s.to_lines, output_id
        );
        let reason = format!(
            "{} output was {} tokens; the summary keeps every failure line (level {level}).",
            info.kind.label(),
            s.from_tokens
        );
        let action = ActionKind::Shape {
            output_id,
            from_tokens: s.from_tokens,
            to_tokens: estimate_tokens(&text),
            from_lines: s.from_lines,
            to_lines: s.to_lines,
        };
        let supported = if failed {
            self.caps.replace_failed_output
        } else {
            self.caps.replace_output
        };
        if self.act_if(d, now, out, action, reason, supported) {
            d.response.replace_output = Some(text);
            if let Some(change) = self.session.on_shaped(info.kind) {
                self.adapt(d, now, out, change);
            }
        }
    }

    fn on_edit_proposed(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::EditProposed {
            tool_use_id,
            path,
            old_text,
            new_text,
        } = event
        else {
            return;
        };
        let (old_hash, new_hash) = (sha256_hex(&old_text), sha256_hex(&new_text));
        let stored = Event::EditProposed {
            tool_use_id,
            path: path.clone(),
            old_text: format!("<blob:{old_hash}>"),
            new_text: format!("<blob:{new_hash}>"),
        };
        d.blobs.push(Blob {
            hash: old_hash,
            content: old_text.clone(),
        });
        d.blobs.push(Blob {
            hash: new_hash,
            content: new_text.clone(),
        });
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        if !is_test_path(&path) {
            return;
        }

        let q = Question::TestWeakened {
            path: &path,
            old: &old_text,
            new: &new_text,
        };
        let (answer, source) = self.broker.ask(&q);
        let (name, version) = (q.name(), q.version());
        let j = self.push(
            d,
            now,
            vec![ev],
            Body::Judgment {
                question: name.into(),
                version,
                subject: path.clone(),
                answer: answer.clone(),
                source,
            },
        );
        let Answer::Weakened { reasons } = answer else {
            return;
        };

        let decision = self.effective_guard(self.session.guard);
        let message = format!(
            "winnow: this edit may weaken tests in {path}: {}.",
            reasons.join("; ")
        );
        let action = ActionKind::Guard { path, decision };
        if self.act(d, now, j, action, message.clone()) {
            match decision {
                GuardDecision::Ask => d.response.permission = Some(Permission::Ask(message)),
                GuardDecision::Deny => d.response.permission = Some(Permission::Deny(message)),
                GuardDecision::Warn => d.response.add_context.push(message),
            }
        }
    }

    fn on_file_edited(&mut self, d: &mut Decision, event: Event, now: u64) {
        let (path, hash) = match &event {
            Event::FileEdited {
                path, after_hash, ..
            } => (path.clone(), after_hash.clone()),
            _ => return,
        };
        let ev = self.push(d, now, vec![], Body::Event { event });
        let Some(hash) = hash else { return };
        if let Some(LoopSignal::Cycle { path }) = self.loops.on_edit(&path, &hash) {
            let text =
                format!("winnow: {path} is back to an earlier version (edit, revert, edit).");
            let reason = format!("Content hash of {path} repeated an earlier state.");
            self.steer(d, now, ev, SteerTrigger::Cycle { path }, text, reason);
        }
    }

    /// Downgrade the wanted guard decision to what the agent supports.
    fn effective_guard(&self, wanted: GuardDecision) -> GuardDecision {
        match wanted {
            GuardDecision::Ask if !self.caps.ask => {
                if self.caps.deny {
                    GuardDecision::Deny
                } else {
                    GuardDecision::Warn
                }
            }
            GuardDecision::Deny if !self.caps.deny => GuardDecision::Warn,
            other => other,
        }
    }

    fn supports(&self, action: &ActionKind) -> bool {
        match action {
            ActionKind::Shape { .. } => self.caps.replace_output,
            ActionKind::Steer { .. } => self.caps.inject_context,
            ActionKind::Guard { decision, .. } => match decision {
                GuardDecision::Ask => self.caps.ask,
                GuardDecision::Deny => self.caps.deny,
                GuardDecision::Warn => self.caps.inject_context,
            },
        }
    }

    /// Record an action. Returns true if the connector must apply it.
    fn act(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        action: ActionKind,
        reason: String,
    ) -> bool {
        let supported = self.supports(&action);
        self.act_if(d, now, cause, action, reason, supported)
    }

    /// `act` with support decided by the caller (it knows more than the action kind).
    fn act_if(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        action: ActionKind,
        reason: String,
        supported: bool,
    ) -> bool {
        let applied = self.session.mode == Mode::Optimize && supported;
        let id = format!("a-{}", self.next_action);
        self.next_action += 1;
        self.push(
            d,
            now,
            vec![cause],
            Body::Action {
                id,
                action,
                reason,
                applied,
                unsupported: !supported,
            },
        );
        applied
    }

    fn steer(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        trigger: SteerTrigger,
        text: String,
        reason: String,
    ) {
        let action = ActionKind::Steer {
            trigger,
            text: text.clone(),
        };
        if self.act(d, now, cause, action, reason) {
            d.response.add_context.push(text);
        }
    }

    fn adapt(&mut self, d: &mut Decision, now: u64, cause: u64, c: Change) {
        self.push(
            d,
            now,
            vec![cause],
            Body::Adaptation {
                knob: c.knob,
                from: c.from,
                to: c.to,
                reason: c.reason,
            },
        );
    }

    fn push(&mut self, d: &mut Decision, now: u64, causes: Vec<u64>, body: Body) -> u64 {
        if self.next_seq == 0 {
            self.next_seq = 1;
            d.entries.push(Entry {
                seq: 0,
                ts_ms: now,
                causes: vec![],
                body: Body::Format {
                    version: LEDGER_FORMAT,
                },
            });
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        d.entries.push(Entry {
            seq,
            ts_ms: now,
            causes,
            body,
        });
        seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::DEFAULT_LEVEL;
    use serde_json::json;

    const PYTEST: &str = include_str!("../../../fixtures/outputs/pytest_fail.txt");

    fn bash(cmd: &str, out: &str) -> Event {
        Event::ToolCompleted {
            tool_use_id: "t".into(),
            tool: "Bash".into(),
            input: json!({ "command": cmd }),
            output: out.into(),
            failed: false,
        }
    }

    fn edited(path: &str, hash: &str) -> Event {
        Event::FileEdited {
            tool_use_id: "e".into(),
            path: path.into(),
            after_hash: Some(hash.into()),
        }
    }

    fn ctl(mode: Mode) -> Controller {
        Controller::new(Config::default(), Capabilities::CLAUDE_CODE, mode)
    }

    #[test]
    fn look_mode_records_but_never_responds() {
        let mut c = ctl(Mode::Look);
        let d = c.handle(bash("pytest", PYTEST), 0);
        assert!(d.response.is_empty());
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                applied: false,
                action: ActionKind::Shape { .. },
                ..
            }
        )));
        assert_eq!(d.blobs.len(), 1, "original output is stored");
    }

    #[test]
    fn optimize_mode_replaces_output_with_footer() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(bash("pytest", PYTEST), 0);
        let text = d.response.replace_output.expect("summary");
        assert!(text.contains("winnow show o-1"));
        assert!(text.contains("FAILED tests/test_api.py::test_get_order"));
        assert!(estimate_tokens(&text) * 3 < estimate_tokens(PYTEST));
    }

    #[test]
    fn failed_run_is_not_shaped_when_the_agent_cannot_replace_it() {
        let mut c = ctl(Mode::Optimize);
        let mut ev = bash("pytest", PYTEST);
        if let Event::ToolCompleted { failed, .. } = &mut ev {
            *failed = true;
        }
        let d = c.handle(ev, 0);
        assert_eq!(d.response.replace_output, None);
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                applied: false,
                unsupported: true,
                action: ActionKind::Shape { .. },
                ..
            }
        )));
    }

    #[test]
    fn reopen_loosens_shape_level() {
        let mut c = ctl(Mode::Optimize);
        c.handle(bash("pytest", PYTEST), 0);
        let d = c.handle(bash("winnow show o-1", PYTEST), 1);
        assert!(d.response.is_empty(), "never shape a re-open");
        assert!(d
            .entries
            .iter()
            .any(|e| matches!(&e.body, Body::Adaptation { .. })));
        assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);
    }

    #[test]
    fn loop_is_steered_after_three_distinct_edits() {
        let mut c = ctl(Mode::Optimize);
        assert!(c
            .handle(bash("pytest", PYTEST), 0)
            .response
            .add_context
            .is_empty());
        for i in 1..=3 {
            c.handle(edited("src/app.py", &format!("h{i}")), i * 60_000);
            let d = c.handle(bash("pytest", PYTEST), i * 60_000);
            let steered = d
                .response
                .add_context
                .iter()
                .any(|t| t.contains("unchanged after 3"));
            assert_eq!(steered, i == 3, "run after edit {i}");
            if i == 3 {
                assert!(d.entries.iter().any(|e| matches!(
                    &e.body,
                    Body::Action {
                        action: ActionKind::Steer {
                            trigger: SteerTrigger::Loop {
                                duration_ms: Some(180_000),
                                ..
                            },
                            ..
                        },
                        ..
                    }
                )));
            }
        }
    }

    #[test]
    fn guard_asks_on_weakened_test_in_claude_code() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(
            Event::EditProposed {
                tool_use_id: "x".into(),
                path: "tests/test_api.py".into(),
                old_text: "assert a == 1\nassert b == 2\n".into(),
                new_text: "assert a == 1\n".into(),
            },
            0,
        );
        assert!(matches!(d.response.permission, Some(Permission::Ask(_))));
    }

    #[test]
    fn guard_downgrades_ask_to_deny_on_codex() {
        let mut c = Controller::new(Config::default(), Capabilities::CODEX, Mode::Optimize);
        let d = c.handle(
            Event::EditProposed {
                tool_use_id: "x".into(),
                path: "tests/test_api.py".into(),
                old_text: "it('a', f)".into(),
                new_text: "it.skip('a', f)".into(),
            },
            0,
        );
        assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
    }

    #[test]
    fn non_default_config_is_recorded_at_session_start() {
        let start = || Event::SessionStarted {
            agent: "a".into(),
            cwd: "/".into(),
            transcript_path: None,
            mode: None,
        };
        let d = ctl(Mode::Look).handle(start(), 0);
        assert!(!d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Config { .. })));

        let cfg = Config {
            loop_threshold: 4,
            ..Config::default()
        };
        let mut c = Controller::new(cfg, Capabilities::CLAUDE_CODE, Mode::Look);
        let d = c.handle(start(), 0);
        let want: BTreeMap<String, String> =
            [("loop_threshold".to_string(), "4".to_string())].into();
        assert!(d
            .entries
            .iter()
            .any(|e| matches!(&e.body, Body::Config { overrides } if *overrides == want)));
    }

    #[test]
    fn first_entry_is_the_format_version() {
        let mut c = ctl(Mode::Look);
        let first = c.handle(edited("a", "h1"), 5).entries;
        assert_eq!(
            first[0].body,
            Body::Format {
                version: LEDGER_FORMAT
            }
        );
        assert_eq!(first[0].seq, 0);
        let next = c.handle(edited("a", "h2"), 6).entries;
        assert!(next.iter().all(|e| !matches!(e.body, Body::Format { .. })));
    }

    #[test]
    fn causes_point_backwards() {
        let mut c = ctl(Mode::Look);
        let mut all = Vec::new();
        all.extend(c.handle(bash("pytest", PYTEST), 0).entries);
        all.extend(c.handle(edited("a", "h1"), 1).entries);
        for (i, e) in all.iter().enumerate() {
            assert_eq!(e.seq, i as u64);
            assert!(e.causes.iter().all(|c| *c < e.seq));
        }
    }

    #[test]
    fn entries_round_trip_through_json() {
        let mut c = ctl(Mode::Optimize);
        for e in c.handle(bash("pytest", PYTEST), 0).entries {
            let s = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Entry>(&s).unwrap(), e);
        }
    }
}
